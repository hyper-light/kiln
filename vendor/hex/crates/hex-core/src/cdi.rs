//! Bounded, pure CDI 1.1 data selection (AC-T7.4.1). No registry or OCI/OS effects.
//! Pinned selector/validator contract and compatibility limits: hex-device-bridge-cdi/READBACK.md.

use std::cell::Cell;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Json,
    Bytes,
    Version,
    Kind,
    Devices,
    Name,
    Annotations,
    ContainerEdits,
    Env,
    DeviceNodes,
    Path,
    HostPath,
    Type,
    Major,
    Minor,
    FileMode,
    Permissions,
    Uid,
    Gid,
    Mounts,
    ContainerPath,
    Options,
    Hooks,
    HookName,
    Args,
    Timeout,
    NetDevices,
    HostInterfaceName,
    IntelRdt,
    ClosId,
    L3CacheSchema,
    MemBwSchema,
    Schemata,
    EnableMonitoring,
    AdditionalGids,
    Selection,
    Unknown,
}

impl Field {
    fn named(name: &str) -> Self {
        match name {
            "cdiVersion" => Self::Version,
            "kind" => Self::Kind,
            "devices" => Self::Devices,
            "name" => Self::Name,
            "annotations" => Self::Annotations,
            "containerEdits" => Self::ContainerEdits,
            "env" => Self::Env,
            "deviceNodes" => Self::DeviceNodes,
            "path" => Self::Path,
            "hostPath" => Self::HostPath,
            "type" => Self::Type,
            "major" => Self::Major,
            "minor" => Self::Minor,
            "fileMode" => Self::FileMode,
            "permissions" => Self::Permissions,
            "uid" => Self::Uid,
            "gid" => Self::Gid,
            "mounts" => Self::Mounts,
            "containerPath" => Self::ContainerPath,
            "options" => Self::Options,
            "hooks" => Self::Hooks,
            "hookName" => Self::HookName,
            "args" => Self::Args,
            "timeout" => Self::Timeout,
            "netDevices" => Self::NetDevices,
            "hostInterfaceName" => Self::HostInterfaceName,
            "intelRdt" => Self::IntelRdt,
            "closID" => Self::ClosId,
            "l3CacheSchema" => Self::L3CacheSchema,
            "memBwSchema" => Self::MemBwSchema,
            "schemata" => Self::Schemata,
            "enableMonitoring" => Self::EnableMonitoring,
            "additionalGids" => Self::AdditionalGids,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V03,
    V04,
    V05,
    V06,
    V07,
    V08,
    V10,
    V11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Limit { bound: usize, actual: usize },
    Allocation,
    MalformedJson,
    UnknownField,
    MissingField,
    DuplicateField,
    UnsupportedValue,
    UnsupportedVersion,
    Requires(Version),
    Empty,
    InvalidName,
    InvalidValue,
    Duplicate,
    Unresolved,
}

/// Coordinates and causes only: never retain a serde message or submitted value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("CDI {field:?}, device {device:?}, item {item:?}: {cause:?}")]
pub struct Error {
    pub field: Field,
    pub device: Option<usize>,
    pub item: Option<usize>,
    pub cause: Cause,
}

impl Error {
    fn new(field: Field, cause: Cause) -> Self {
        Self {
            field,
            device: None,
            item: None,
            cause,
        }
    }
    fn at(mut self, device: Option<usize>, item: Option<usize>) -> Self {
        self.device = device;
        self.item = item;
        self
    }
}

impl de::Error for Error {
    // The fixed schema's only custom deserializer errors are fallible reservations below.
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self::new(Field::Json, Cause::Allocation)
    }
    fn invalid_type(_: de::Unexpected<'_>, _: &dyn de::Expected) -> Self {
        Self::new(Field::Json, Cause::InvalidValue)
    }
    fn invalid_value(_: de::Unexpected<'_>, _: &dyn de::Expected) -> Self {
        Self::new(Field::Json, Cause::InvalidValue)
    }
    fn invalid_length(_: usize, _: &dyn de::Expected) -> Self {
        Self::new(Field::Json, Cause::InvalidValue)
    }
    fn unknown_variant(_: &str, _: &'static [&'static str]) -> Self {
        Self::new(Field::Json, Cause::UnsupportedValue)
    }
    fn unknown_field(_: &str, _: &'static [&'static str]) -> Self {
        Self::new(Field::Unknown, Cause::UnknownField)
    }
    fn missing_field(field: &'static str) -> Self {
        Self::new(Field::named(field), Cause::MissingField)
    }
    fn duplicate_field(field: &'static str) -> Self {
        Self::new(Field::named(field), Cause::DuplicateField)
    }
}

// Keep schema/collection errors typed across serde_json's fixed error type without parsing its
// text. Every native JSON error becomes MalformedJson; the side channel holds no private bytes.
struct Decode<'a, D> {
    inner: D,
    error: &'a Cell<Option<Error>>,
}
struct Capture<'a, V> {
    inner: V,
    error: &'a Cell<Option<Error>>,
    string: bool,
}
struct Seed<'a, S> {
    inner: S,
    error: &'a Cell<Option<Error>>,
}
struct Access<'a, A> {
    inner: A,
    error: &'a Cell<Option<Error>>,
}

fn capture<E: de::Error>(slot: &Cell<Option<Error>>, error: Error) -> E {
    slot.set(Some(error));
    E::custom("CDI input refused")
}
fn decoded(slot: &Cell<Option<Error>>) -> Error {
    slot.take()
        .unwrap_or(Error::new(Field::Json, Cause::MalformedJson))
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Decode<'_, D> {
    type Error = Error;
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .deserialize_any(Capture {
                inner: visitor,
                error: self.error,
                string: false,
            })
            .map_err(|_| decoded(self.error))
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .deserialize_string(Capture {
                inner: visitor,
                error: self.error,
                string: true,
            })
            .map_err(|_| decoded(self.error))
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .deserialize_option(Capture {
                inner: visitor,
                error: self.error,
                string: false,
            })
            .map_err(|_| decoded(self.error))
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.inner
            .deserialize_map(Capture {
                inner: visitor,
                error: self.error,
                string: false,
            })
            .map_err(|_| decoded(self.error))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str bytes byte_buf unit
        unit_struct newtype_struct seq tuple tuple_struct enum identifier ignored_any
    }
}

macro_rules! scalar {
    ($method:ident, $ty:ty) => {
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            self.inner
                .$method::<Error>(value)
                .map_err(|e| capture(self.error, e))
        }
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Capture<'_, V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.expecting(f)
    }
    scalar!(visit_bool, bool);
    scalar!(visit_i64, i64);
    scalar!(visit_u64, u64);
    scalar!(visit_f64, f64);
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if self.string {
            let mut owned = String::new();
            owned
                .try_reserve_exact(value.len())
                .map_err(|_| capture(self.error, Error::new(Field::Json, Cause::Allocation)))?;
            owned.push_str(value);
            self.inner
                .visit_string::<Error>(owned)
                .map_err(|e| capture(self.error, e))
        } else {
            self.inner
                .visit_str::<Error>(value)
                .map_err(|e| capture(self.error, e))
        }
    }
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        self.visit_str(value)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner
            .visit_unit::<Error>()
            .map_err(|e| capture(self.error, e))
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.inner
            .visit_none::<Error>()
            .map_err(|e| capture(self.error, e))
    }
    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner
            .visit_some(Decode {
                inner,
                error: self.error,
            })
            .map_err(|e| capture(self.error, e))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_seq(Access {
                inner,
                error: self.error,
            })
            .map_err(|e| capture(self.error, e))
    }
    fn visit_map<A: MapAccess<'de>>(self, inner: A) -> Result<Self::Value, A::Error> {
        self.inner
            .visit_map(Access {
                inner,
                error: self.error,
            })
            .map_err(|e| capture(self.error, e))
    }
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<'_, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
        self.inner
            .deserialize(Decode {
                inner,
                error: self.error,
            })
            .map_err(|e| capture(self.error, e))
    }
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for Access<'_, A> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        inner: K,
    ) -> Result<Option<K::Value>, Error> {
        self.inner
            .next_key_seed(Seed {
                inner,
                error: self.error,
            })
            .map_err(|_| decoded(self.error))
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, inner: V) -> Result<V::Value, Error> {
        self.inner
            .next_value_seed(Seed {
                inner,
                error: self.error,
            })
            .map_err(|_| decoded(self.error))
    }
}
impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Access<'_, A> {
    type Error = Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        inner: T,
    ) -> Result<Option<T::Value>, Error> {
        self.inner
            .next_element_seed(Seed {
                inner,
                error: self.error,
            })
            .map_err(|_| decoded(self.error))
    }
}

fn list<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Vec<T>, D::Error> {
    struct List<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for List<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = a.next_element()? {
                values
                    .try_reserve(1)
                    .map_err(|_| de::Error::custom("allocation"))?;
                values.push(value);
            }
            Ok(values)
        }
    }
    d.deserialize_seq(List(std::marker::PhantomData))
}

#[derive(PartialEq, Eq)]
pub struct Annotation {
    pub key: String,
    pub value: String,
}
fn annotations<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Annotation>, D::Error> {
    struct Map;
    impl<'de> Visitor<'de> for Map {
        type Value = Vec<Annotation>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("string metadata")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some((key, value)) = a.next_entry()? {
                values
                    .try_reserve(1)
                    .map_err(|_| de::Error::custom("allocation"))?;
                values.push(Annotation { key, value });
            }
            Ok(values)
        }
    }
    d.deserialize_map(Map)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Unspecified,
    Block,
    Char,
    UnbufferedChar,
    Fifo,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookName {
    Prestart,
    CreateRuntime,
    CreateContainer,
    StartContainer,
    Poststart,
    Poststop,
}
macro_rules! string_enum {
    ($ty:ty, $($text:literal => $variant:ident),+ $(,)?) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = String::deserialize(d)?;
                match value.as_str() {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(de::Error::unknown_variant("", &[])),
                }
            }
        }
    };
}
string_enum!(DeviceType, "" => Unspecified, "b" => Block, "c" => Char, "u" => UnbufferedChar, "p" => Fifo);
string_enum!(HookName, "prestart" => Prestart, "createRuntime" => CreateRuntime,
    "createContainer" => CreateContainer, "startContainer" => StartContainer,
    "poststart" => Poststart, "poststop" => Poststop);

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceNode {
    pub path: String,
    pub host_path: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<DeviceType>,
    pub major: Option<i64>,
    pub minor: Option<i64>,
    pub file_mode: Option<u32>,
    pub permissions: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mount {
    pub host_path: String,
    pub container_path: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "list")]
    pub options: Vec<String>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hook {
    pub hook_name: HookName,
    pub path: String,
    #[serde(default, deserialize_with = "list")]
    pub args: Vec<String>,
    #[serde(default, deserialize_with = "list")]
    pub env: Vec<String>,
    pub timeout: Option<i64>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetDevice {
    pub host_interface_name: String,
    pub name: String,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntelRdt {
    #[serde(default, rename = "closID")]
    pub clos_id: String,
    #[serde(default)]
    pub l3_cache_schema: String,
    #[serde(default)]
    pub mem_bw_schema: String,
    #[serde(default, deserialize_with = "optional_list")]
    pub schemata: Option<Vec<String>>,
    #[serde(default)]
    pub enable_monitoring: bool,
}
fn optional_list<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    struct Strings(Vec<String>);
    impl<'de> Deserialize<'de> for Strings {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            list(d).map(Self)
        }
    }
    Option::<Strings>::deserialize(d).map(|v| v.map(|v| v.0))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawEdits {
    #[serde(default, deserialize_with = "list")]
    env: Vec<String>,
    #[serde(default, deserialize_with = "list")]
    device_nodes: Vec<DeviceNode>,
    #[serde(default, deserialize_with = "list")]
    mounts: Vec<Mount>,
    #[serde(default, deserialize_with = "list")]
    hooks: Vec<Hook>,
    #[serde(default, deserialize_with = "list")]
    net_devices: Vec<NetDevice>,
    intel_rdt: Option<IntelRdt>,
    #[serde(default, deserialize_with = "list")]
    additional_gids: Vec<u32>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Device {
    name: String,
    #[serde(default, deserialize_with = "annotations")]
    annotations: Vec<Annotation>,
    container_edits: RawEdits,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawSpec {
    cdi_version: String,
    kind: String,
    #[serde(default, deserialize_with = "annotations")]
    annotations: Vec<Annotation>,
    #[serde(deserialize_with = "list")]
    devices: Vec<Device>,
    #[serde(default)]
    container_edits: RawEdits,
}

pub struct Spec {
    raw: RawSpec,
    version: Version,
    bytes: usize,
}
/// Ordered Go Append data, not a partially applied OCI configuration.
#[derive(Default)]
pub struct Edits<'s> {
    pub env: Vec<&'s str>,
    pub device_nodes: Vec<&'s DeviceNode>,
    pub mounts: Vec<&'s Mount>,
    pub hooks: Vec<&'s Hook>,
    pub net_devices: Vec<&'s NetDevice>,
    pub intel_rdt: Option<&'s IntelRdt>,
    pub additional_gids: Vec<u32>,
}
macro_rules! redacted {
    ($($ty:ty),+ $(,)?) => {$(
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("CDI(<redacted>)") }
        }
    )+};
}
redacted!(
    Spec,
    Edits<'_>,
    Annotation,
    DeviceNode,
    Mount,
    Hook,
    NetDevice,
    IntelRdt
);

impl Spec {
    pub fn parse(bytes: &[u8], max_bytes: usize) -> Result<Self, Error> {
        if bytes.len() > max_bytes {
            return Err(Error::new(
                Field::Bytes,
                Cause::Limit {
                    bound: max_bytes,
                    actual: bytes.len(),
                },
            ));
        }
        let error = Cell::new(None);
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let raw = RawSpec::deserialize(Decode {
            inner: &mut decoder,
            error: &error,
        })?;
        decoder
            .end()
            .map_err(|_| Error::new(Field::Json, Cause::MalformedJson))?;
        let version = match raw.cdi_version.as_str() {
            "0.3.0" => Version::V03,
            "0.4.0" => Version::V04,
            "0.5.0" => Version::V05,
            "0.6.0" => Version::V06,
            "0.7.0" => Version::V07,
            "0.8.0" => Version::V08,
            "1.0.0" => Version::V10,
            "1.1.0" => Version::V11,
            _ => return Err(Error::new(Field::Version, Cause::UnsupportedVersion)),
        };
        let result = Self {
            raw,
            version,
            bytes: bytes.len(),
        };
        result.validate()?;
        Ok(result)
    }

    pub fn select<'s>(
        &'s self,
        qualified: &[&str],
        max_selected: usize,
    ) -> Result<Edits<'s>, Error> {
        if qualified.len() > max_selected {
            return Err(Error::new(
                Field::Selection,
                Cause::Limit {
                    bound: max_selected,
                    actual: qualified.len(),
                },
            ));
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(qualified.len())
            .map_err(|_| Error::new(Field::Selection, Cause::Allocation))?;
        for (index, name) in qualified.iter().enumerate() {
            let refusal = |cause| Error::new(Field::Selection, cause).at(None, Some(index));
            // A resolvable qualified name cannot be longer than its source JSON document.
            if name.len() > self.bytes {
                return Err(refusal(Cause::Unresolved));
            }
            let (kind, device) = name
                .split_once('=')
                .ok_or_else(|| refusal(Cause::InvalidName))?;
            if !valid_kind(kind) || !valid_name(device, true) {
                return Err(refusal(Cause::InvalidName));
            }
            if qualified.iter().take(index).any(|prior| prior == name) {
                return Err(refusal(Cause::Duplicate));
            }
            if kind != self.raw.kind {
                return Err(refusal(Cause::Unresolved));
            }
            let found = self
                .raw
                .devices
                .iter()
                .find(|d| d.name == device)
                .ok_or_else(|| refusal(Cause::Unresolved))?;
            devices.push(found);
        }
        let mut result = Edits::default();
        if !devices.is_empty() {
            result.append(&self.raw.container_edits)?;
        }
        for device in devices {
            result.append(&device.container_edits)?;
        }
        Ok(result)
    }

    fn require(
        &self,
        used: bool,
        version: Version,
        field: Field,
        device: Option<usize>,
    ) -> Result<(), Error> {
        if used && self.version < version {
            return Err(Error::new(field, Cause::Requires(version)).at(device, None));
        }
        Ok(())
    }
    fn validate(&self) -> Result<(), Error> {
        if !valid_kind(&self.raw.kind) {
            return Err(Error::new(Field::Kind, Cause::InvalidName));
        }
        let (_, class) = self
            .raw
            .kind
            .split_once('/')
            .ok_or_else(|| Error::new(Field::Kind, Cause::InvalidName))?;
        self.require(class.contains('.'), Version::V06, Field::Kind, None)?;
        self.metadata(&self.raw.annotations, None)?;
        if self.raw.devices.is_empty() {
            return Err(Error::new(Field::Devices, Cause::Empty));
        }
        self.validate_edits(&self.raw.container_edits, None)?;
        for (index, device) in self.raw.devices.iter().enumerate() {
            if !valid_name(&device.name, true) {
                return Err(Error::new(Field::Name, Cause::InvalidName).at(Some(index), None));
            }
            if self
                .raw
                .devices
                .iter()
                .take(index)
                .any(|d| d.name == device.name)
            {
                return Err(Error::new(Field::Name, Cause::Duplicate).at(Some(index), None));
            }
            self.require(
                device
                    .name
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_digit()),
                Version::V05,
                Field::Name,
                Some(index),
            )?;
            self.metadata(&device.annotations, Some(index))?;
            let edits = &device.container_edits;
            if edits.env.is_empty()
                && edits.device_nodes.is_empty()
                && edits.mounts.is_empty()
                && edits.hooks.is_empty()
                && edits.net_devices.is_empty()
                && edits.intel_rdt.is_none()
                && edits.additional_gids.is_empty()
            {
                return Err(Error::new(Field::ContainerEdits, Cause::Empty).at(Some(index), None));
            }
            self.validate_edits(edits, Some(index))?;
        }
        Ok(())
    }
    fn metadata(&self, rows: &[Annotation], device: Option<usize>) -> Result<(), Error> {
        self.require(!rows.is_empty(), Version::V06, Field::Annotations, device)?;
        for (index, row) in rows.iter().enumerate() {
            if rows.iter().take(index).any(|prior| prior.key == row.key) {
                return Err(
                    Error::new(Field::Annotations, Cause::Duplicate).at(device, Some(index))
                );
            }
        }
        Ok(())
    }
    fn validate_edits(&self, edits: &RawEdits, device: Option<usize>) -> Result<(), Error> {
        env(&edits.env, device)?;
        self.require(
            !edits.additional_gids.is_empty(),
            Version::V07,
            Field::AdditionalGids,
            device,
        )?;
        self.require(
            !edits.net_devices.is_empty(),
            Version::V11,
            Field::NetDevices,
            device,
        )?;
        for (index, node) in edits.device_nodes.iter().enumerate() {
            nonempty(&node.path, Field::Path, device, index)?;
            self.require(
                node.host_path.as_ref().is_some_and(|p| !p.is_empty()),
                Version::V05,
                Field::HostPath,
                device,
            )?;
            if node
                .permissions
                .as_ref()
                .is_some_and(|p| !p.bytes().all(|c| matches!(c, b'r' | b'w' | b'm')))
            {
                return Err(
                    Error::new(Field::Permissions, Cause::InvalidValue).at(device, Some(index))
                );
            }
        }
        for (index, mount) in edits.mounts.iter().enumerate() {
            nonempty(&mount.host_path, Field::HostPath, device, index)?;
            nonempty(&mount.container_path, Field::ContainerPath, device, index)?;
            self.require(
                mount.kind.as_ref().is_some_and(|k| !k.is_empty()),
                Version::V04,
                Field::Type,
                device,
            )?;
        }
        for (index, hook) in edits.hooks.iter().enumerate() {
            if !hook.path.starts_with('/') {
                return Err(Error::new(Field::Path, Cause::InvalidValue).at(device, Some(index)));
            }
            if hook.timeout.is_some_and(|t| t <= 0) {
                return Err(Error::new(Field::Timeout, Cause::InvalidValue).at(device, Some(index)));
            }
            env(&hook.env, device)?;
        }
        for (index, net) in edits.net_devices.iter().enumerate() {
            nonempty(
                &net.host_interface_name,
                Field::HostInterfaceName,
                device,
                index,
            )?;
            nonempty(&net.name, Field::Name, device, index)?;
            if edits
                .net_devices
                .iter()
                .take(index)
                .any(|old| old.host_interface_name == net.host_interface_name)
            {
                return Err(
                    Error::new(Field::HostInterfaceName, Cause::Duplicate).at(device, Some(index))
                );
            }
            if edits
                .net_devices
                .iter()
                .take(index)
                .any(|old| old.name == net.name)
            {
                return Err(Error::new(Field::Name, Cause::Duplicate).at(device, Some(index)));
            }
        }
        if let Some(rdt) = &edits.intel_rdt {
            self.require(true, Version::V07, Field::IntelRdt, device)?;
            self.require(
                rdt.schemata.is_some() || rdt.enable_monitoring,
                Version::V11,
                Field::IntelRdt,
                device,
            )?;
            // The pinned CDI validator's Linux filename bound, not a runtime tuning constant.
            if rdt.clos_id != "/"
                && (rdt.clos_id.len() >= 4096
                    || matches!(rdt.clos_id.as_str(), "." | "..")
                    || rdt.clos_id.contains(['/', '\n']))
            {
                return Err(Error::new(Field::ClosId, Cause::InvalidValue).at(device, None));
            }
        }
        Ok(())
    }
}

impl<'s> Edits<'s> {
    fn append(&mut self, edits: &'s RawEdits) -> Result<(), Error> {
        reserve(&mut self.env, edits.env.len(), Field::Env)?;
        reserve(
            &mut self.device_nodes,
            edits.device_nodes.len(),
            Field::DeviceNodes,
        )?;
        reserve(&mut self.mounts, edits.mounts.len(), Field::Mounts)?;
        reserve(&mut self.hooks, edits.hooks.len(), Field::Hooks)?;
        reserve(
            &mut self.net_devices,
            edits.net_devices.len(),
            Field::NetDevices,
        )?;
        reserve(
            &mut self.additional_gids,
            edits.additional_gids.len(),
            Field::AdditionalGids,
        )?;
        self.env.extend(edits.env.iter().map(String::as_str));
        self.device_nodes.extend(edits.device_nodes.iter());
        self.mounts.extend(edits.mounts.iter());
        self.hooks.extend(edits.hooks.iter());
        self.net_devices.extend(edits.net_devices.iter());
        self.additional_gids
            .extend(edits.additional_gids.iter().copied());
        if edits.intel_rdt.is_some() {
            self.intel_rdt = edits.intel_rdt.as_ref();
        }
        Ok(())
    }
}
fn reserve<T>(out: &mut Vec<T>, count: usize, field: Field) -> Result<(), Error> {
    out.try_reserve_exact(count)
        .map_err(|_| Error::new(field, Cause::Allocation))
}
fn nonempty(value: &str, field: Field, device: Option<usize>, item: usize) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::new(field, Cause::Empty).at(device, Some(item)));
    }
    Ok(())
}
fn env(values: &[String], device: Option<usize>) -> Result<(), Error> {
    for (index, value) in values.iter().enumerate() {
        if !value
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty())
        {
            return Err(Error::new(Field::Env, Cause::InvalidValue).at(device, Some(index)));
        }
    }
    Ok(())
}
fn valid_kind(kind: &str) -> bool {
    kind.split_once('/')
        .is_some_and(|(vendor, class)| valid_name(vendor, false) && valid_name(class, false))
}
fn valid_name(value: &str, device: bool) -> bool {
    value.bytes().next().is_some_and(|b| {
        if device {
            b.is_ascii_alphanumeric()
        } else {
            b.is_ascii_alphabetic()
        }
    }) && value
        .bytes()
        .last()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') || (device && b == b':')
        })
}

#[cfg(test)]
mod tests;
