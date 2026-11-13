//! Only the existing GpuObserverInput schema, with fallible owned strings/arrays.
//! RFC8259 strings include escaped Unicode; nesting is fixed by these readers,
//! never a recursive JSON value or a second observer/configuration authority.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use hex_api::{Bounds, IoRates, Refusal};
use sha2::{Digest as _, Sha256};

use super::super::{exhausted, hex_array, invalid};
use super::{GpuArtifact, GpuArtifactRole, GpuObserverInput};

pub const ENV_GPU_OBSERVER_FILE: &str = "HEX_GUEST_GPU_OBSERVER_FILE";
pub const ENV_GPU_OBSERVER_SHA256: &str = "HEX_GUEST_GPU_OBSERVER_SHA256";
const MAX_BYTES: usize = hex_core::bundle::MAX_CONFIG_BYTES;

impl GpuObserverInput {
    /// No input leaves CPU-only configuration unchanged. A partial pair refuses;
    /// an input is never synthesized from PATH, catalog metadata or a default.
    pub fn from_env() -> Result<Option<Self>, Refusal> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Option<Self>, Refusal> {
        match (var(ENV_GPU_OBSERVER_FILE), var(ENV_GPU_OBSERVER_SHA256)) {
            (None, None) => Ok(None),
            (Some(file), Some(hash)) => {
                let hash = hash
                    .to_str()
                    .ok_or_else(|| invalid("observer configuration digest is not ASCII"))?;
                Self::load(Path::new(&file), digest(hash)?).map(Some)
            }
            _ => Err(invalid(
                "observer configuration requires both file and SHA-256",
            )),
        }
    }

    /// Read only explicit bounded configuration bytes. The caller must pass
    /// this input to the existing GpuObserver constructor before publication;
    /// that constructor verifies the actual artifacts, mappings and paid room.
    pub fn load(file: &Path, expected_sha256: [u8; 32]) -> Result<Self, Refusal> {
        let bytes = read(file)?;
        let actual: [u8; 32] = Sha256::digest(&bytes).into();
        if actual != expected_sha256 {
            return Err(invalid(
                "observer configuration SHA-256 differs from its input",
            ));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid("observer configuration is not UTF-8"))?;
        parse(text)
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Refusal> {
    if !path.is_absolute() {
        return Err(invalid(
            "observer configuration path must be explicit and absolute",
        ));
    }
    if !std::fs::symlink_metadata(path)
        .map_err(|why| Refusal::os("locating observer configuration", why))?
        .is_file()
    {
        return Err(invalid("observer configuration must be a regular file"));
    }
    #[cfg(unix)]
    let mut file = {
        use rustix::fs::{Mode, OFlags};
        let descriptor = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|why| Refusal::os("opening observer configuration", std::io::Error::from(why)))?;
        std::fs::File::from(descriptor)
    };
    #[cfg(windows)]
    let mut file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|why| Refusal::os("opening observer configuration", why))?
    };
    let before = file
        .metadata()
        .map_err(|why| Refusal::os("sizing observer configuration", why))?;
    if !before.is_file() {
        return Err(invalid(
            "opened observer configuration is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if before.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(invalid("observer configuration cannot be a reparse point"));
        }
    }
    let identity = hex_client::guest::stage::identity(&before);
    let size = usize::try_from(before.len())
        .ok()
        .filter(|size| *size <= MAX_BYTES)
        .ok_or_else(|| exhausted(MAX_BYTES))?;
    let room = size.checked_add(1).ok_or_else(|| exhausted(MAX_BYTES))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| exhausted(MAX_BYTES))?;
    bytes.resize(room, 0);
    let mut count = 0usize;
    while count < room {
        let got = file
            .read(bytes.get_mut(count..).ok_or_else(|| exhausted(MAX_BYTES))?)
            .map_err(|why| Refusal::os("reading observer configuration", why))?;
        if got == 0 {
            break;
        }
        count = count.checked_add(got).ok_or_else(|| exhausted(MAX_BYTES))?;
    }
    let after = file
        .metadata()
        .map_err(|why| Refusal::os("checking observer configuration identity", why))?;
    if count != size
        || after.len() != before.len()
        || hex_client::guest::stage::identity(&after) != identity
    {
        return Err(invalid(
            "observer configuration changed during its bounded read",
        ));
    }
    bytes.truncate(count);
    Ok(bytes)
}

fn digest(value: &str) -> Result<[u8; 32], Refusal> {
    // SHA-256's 256 bits, encoded as two ASCII digits for each of its32 bytes.
    let mut lower = [0u8; 64];
    if value.len() != lower.len() || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "observer digest needs exactly64 ASCII hexadecimal digits",
        ));
    }
    for (to, from) in lower.iter_mut().zip(value.bytes()) {
        *to = from.to_ascii_lowercase();
    }
    hex_array(std::str::from_utf8(&lower).map_err(|_| invalid("observer digest is not ASCII"))?)
}

/// Finite cursor into one MAX_CONFIG_BYTES input. Only this schema's string,
/// unsigned integer and null tokens exist; there is no recursive value reader.
struct Json<'a> {
    rest: &'a str,
    bytes: usize,
}

impl Json<'_> {
    fn space(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
    }
    fn take(&mut self, value: char) -> bool {
        self.space();
        if let Some(tail) = self.rest.strip_prefix(value) {
            self.rest = tail;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, value: char) -> Result<(), Refusal> {
        if self.take(value) {
            Ok(())
        } else {
            Err(invalid("observer JSON delimiter is missing"))
        }
    }
    fn null(&mut self) -> bool {
        self.space();
        if let Some(tail) = self.rest.strip_prefix("null") {
            self.rest = tail;
            true
        } else {
            false
        }
    }
    fn number(&mut self) -> Result<u64, Refusal> {
        self.space();
        let count = self.rest.bytes().take_while(u8::is_ascii_digit).count();
        let token = self.rest.get(..count).ok_or_else(|| exhausted(MAX_BYTES))?;
        if token.is_empty() || (token.len() > 1 && token.starts_with('0')) {
            return Err(invalid("observer JSON requires an unsigned integer"));
        }
        let number = token
            .parse()
            .map_err(|_| invalid("observer integer exceeds u64"))?;
        self.rest = self.rest.get(count..).ok_or_else(|| exhausted(MAX_BYTES))?;
        Ok(number)
    }
    fn optional<T: TryFrom<u64>>(&mut self) -> Result<Option<T>, Refusal> {
        if self.null() {
            Ok(None)
        } else {
            narrow(self.number()?).map(Some)
        }
    }
    fn string(&mut self) -> Result<String, Refusal> {
        self.expect('"')?;
        let mut escaped = false;
        let end = self
            .rest
            .char_indices()
            .find_map(|(offset, character)| {
                if character == '"' && !escaped {
                    return Some(offset);
                }
                escaped = character == '\\' && !escaped;
                None
            })
            .ok_or_else(|| invalid("observer JSON string is unterminated"))?;
        let raw = self.rest.get(..end).ok_or_else(|| exhausted(MAX_BYTES))?;
        self.rest = self
            .rest
            .get(end.checked_add(1).ok_or_else(|| exhausted(MAX_BYTES))?..)
            .ok_or_else(|| exhausted(MAX_BYTES))?;
        let mut value = String::new();
        value
            .try_reserve_exact(raw.len())
            .map_err(|_| exhausted(MAX_BYTES))?;
        let mut characters = raw.chars();
        while let Some(character) = characters.next() {
            let character = match character {
                '\\' => match characters.next() {
                    Some('"') => '"',
                    Some('\\') => '\\',
                    Some('/') => '/',
                    Some('b') => '\u{0008}',
                    Some('f') => '\u{000c}',
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some('u') => unicode(&mut characters)?,
                    _ => return Err(invalid("observer JSON string escape is invalid")),
                },
                character if character < '\u{0020}' => {
                    return Err(invalid("observer JSON string has a raw control byte"));
                }
                character => character,
            };
            // RFC8259 decoding never grows beyond the raw quoted byte count.
            value.push(character);
        }
        Ok(value)
    }
}

fn hex4(characters: &mut std::str::Chars<'_>) -> Result<u32, Refusal> {
    let mut scalar = 0u32;
    for _ in 0..4 {
        let digit = characters
            .next()
            .filter(char::is_ascii_hexdigit)
            .and_then(|character| character.to_digit(16))
            .ok_or_else(|| invalid("observer JSON Unicode escape needs four hex digits"))?;
        scalar = scalar
            .checked_mul(16)
            .and_then(|value| value.checked_add(digit))
            .ok_or_else(|| invalid("observer Unicode scalar overflows"))?;
    }
    Ok(scalar)
}

fn unicode(characters: &mut std::str::Chars<'_>) -> Result<char, Refusal> {
    let first = hex4(characters)?;
    let scalar = if (0xd800..=0xdbff).contains(&first) {
        if characters.next() != Some('\\') || characters.next() != Some('u') {
            return Err(invalid(
                "observer JSON high surrogate lacks its low surrogate",
            ));
        }
        let second = hex4(characters)?;
        if !(0xdc00..=0xdfff).contains(&second) {
            return Err(invalid("observer JSON low surrogate is invalid"));
        }
        first
            .checked_sub(0xd800)
            .and_then(|value| value.checked_mul(0x400))
            .and_then(|value| value.checked_add(second.checked_sub(0xdc00)?))
            .and_then(|value| value.checked_add(0x10000))
            .ok_or_else(|| invalid("observer JSON surrogate pair overflows"))?
    } else {
        first
    };
    char::from_u32(scalar).ok_or_else(|| invalid("observer JSON Unicode scalar is invalid"))
}

enum Position {
    First,
    More,
    End,
}
struct Members {
    close: char,
    position: Position,
}
impl Members {
    fn begin(json: &mut Json<'_>, open: char, close: char) -> Result<Self, Refusal> {
        json.expect(open)?;
        Ok(Self {
            close,
            position: Position::First,
        })
    }
    fn next(&mut self, json: &mut Json<'_>) -> Result<bool, Refusal> {
        if matches!(self.position, Position::End) {
            return Ok(false);
        }
        if json.take(self.close) {
            self.position = Position::End;
            return Ok(false);
        }
        if matches!(self.position, Position::More) {
            json.expect(',')?;
        }
        self.position = Position::More;
        Ok(true)
    }
    fn field(&mut self, json: &mut Json<'_>) -> Result<Option<String>, Refusal> {
        if !self.next(json)? {
            return Ok(None);
        }
        let field = json.string()?;
        json.expect(':')?;
        Ok(Some(field))
    }
}

fn narrow<T: TryFrom<u64>>(value: u64) -> Result<T, Refusal> {
    T::try_from(value).map_err(|_| invalid("observer integer exceeds its native field width"))
}
fn put<T>(slot: &mut Option<T>, value: T) -> Result<(), Refusal> {
    if slot.replace(value).is_some() {
        Err(invalid("observer JSON field is duplicated"))
    } else {
        Ok(())
    }
}
fn required<T>(slot: Option<T>) -> Result<T, Refusal> {
    slot.ok_or_else(|| invalid("observer JSON required field is missing"))
}
fn seen(mask: &mut u8, bit: u8) -> Result<(), Refusal> {
    if *mask & bit != 0 {
        return Err(invalid("observer JSON bound is duplicated"));
    }
    *mask |= bit;
    Ok(())
}
fn reserve<T>(values: &mut Vec<T>, json: &Json<'_>) -> Result<(), Refusal> {
    values
        .len()
        .checked_add(1)
        .filter(|count| *count <= json.bytes)
        .and_then(|count| count.checked_mul(size_of::<T>()))
        .ok_or_else(|| exhausted(MAX_BYTES))?;
    values.try_reserve(1).map_err(|_| exhausted(MAX_BYTES))
}

fn rates(json: &mut Json<'_>) -> Result<IoRates, Refusal> {
    let mut fields = Members::begin(json, '{', '}')?;
    let mut rates = IoRates::default();
    let mut mask = 0;
    while let Some(field) = fields.field(json)? {
        match field.as_str() {
            "read_bps" => {
                seen(&mut mask, 1)?;
                rates.read_bps = json.optional()?;
            }
            "write_bps" => {
                seen(&mut mask, 2)?;
                rates.write_bps = json.optional()?;
            }
            "read_iops" => {
                seen(&mut mask, 4)?;
                rates.read_iops = json.optional()?;
            }
            "write_iops" => {
                seen(&mut mask, 8)?;
                rates.write_iops = json.optional()?;
            }
            _ => return Err(invalid("observer JSON has an unknown IO bound")),
        }
    }
    if mask != 0x0f {
        return Err(invalid("observer JSON IO bounds must be explicit"));
    }
    Ok(rates)
}

fn bounds(json: &mut Json<'_>) -> Result<Bounds, Refusal> {
    let mut fields = Members::begin(json, '{', '}')?;
    let mut bounds = Bounds::default();
    let mut mask = 0;
    while let Some(field) = fields.field(json)? {
        match field.as_str() {
            "cpu_weight" => {
                seen(&mut mask, 1)?;
                bounds.cpu_weight = json.optional()?;
            }
            "cpu_max_milli" => {
                seen(&mut mask, 2)?;
                bounds.cpu_max_milli = json.optional()?;
            }
            "memory_max" => {
                seen(&mut mask, 4)?;
                bounds.memory_max = json.optional()?;
            }
            "memory_high" => {
                seen(&mut mask, 8)?;
                bounds.memory_high = json.optional()?;
            }
            "io_weight" => {
                seen(&mut mask, 16)?;
                bounds.io_weight = json.optional()?;
            }
            "io_latency_us" => {
                seen(&mut mask, 32)?;
                bounds.io_latency_us = json.optional()?;
            }
            "io_max" => {
                seen(&mut mask, 64)?;
                bounds.io_max = if json.null() {
                    None
                } else {
                    Some(rates(json)?)
                };
            }
            "pids_max" => {
                seen(&mut mask, 128)?;
                bounds.pids_max = json.optional()?;
            }
            _ => return Err(invalid("observer JSON has an unknown workload bound")),
        }
    }
    if mask != 0xff {
        return Err(invalid("observer JSON workload bounds must be explicit"));
    }
    Ok(bounds)
}

fn artifact(json: &mut Json<'_>) -> Result<GpuArtifact, Refusal> {
    let mut fields = Members::begin(json, '{', '}')?;
    let (mut role, mut host, mut guest, mut bytes, mut hash) = (None, None, None, None, None);
    while let Some(field) = fields.field(json)? {
        match field.as_str() {
            "role" => put(
                &mut role,
                match json.string()?.as_str() {
                    "program" => GpuArtifactRole::Program,
                    "interpreter" => GpuArtifactRole::Interpreter,
                    "loader" => GpuArtifactRole::Loader,
                    "icd" => GpuArtifactRole::Icd,
                    "driver" => GpuArtifactRole::Driver,
                    "dependency" => GpuArtifactRole::Dependency,
                    _ => return Err(invalid("observer JSON artifact role is unknown")),
                },
            )?,
            "host" => {
                let value = json.string()?;
                let mut path = PathBuf::new();
                path.try_reserve_exact(value.len())
                    .map_err(|_| exhausted(MAX_BYTES))?;
                path.push(value);
                put(&mut host, path)?;
            }
            "guest" => put(&mut guest, json.string()?)?,
            "bytes" => put(&mut bytes, json.number()?)?,
            "sha256" => put(&mut hash, digest(&json.string()?)?)?,
            _ => return Err(invalid("observer JSON artifact field is unknown")),
        }
    }
    Ok(GpuArtifact {
        role: required(role)?,
        host: required(host)?,
        guest: required(guest)?,
        bytes: required(bytes)?,
        sha256: required(hash)?,
    })
}

fn artifacts(json: &mut Json<'_>) -> Result<Vec<GpuArtifact>, Refusal> {
    let mut members = Members::begin(json, '[', ']')?;
    let mut values = Vec::new();
    while members.next(json)? {
        reserve(&mut values, json)?;
        values.push(artifact(json)?);
    }
    Ok(values)
}
fn launch(json: &mut Json<'_>) -> Result<Vec<String>, Refusal> {
    let mut members = Members::begin(json, '[', ']')?;
    let mut values = Vec::new();
    while members.next(json)? {
        reserve(&mut values, json)?;
        values.push(json.string()?);
    }
    Ok(values)
}

fn parse(text: &str) -> Result<GpuObserverInput, Refusal> {
    if text.len() > MAX_BYTES {
        return Err(exhausted(MAX_BYTES));
    }
    let mut json = Json {
        rest: text,
        bytes: text.len(),
    };
    let mut fields = Members::begin(&mut json, '{', '}')?;
    let (mut evidence, mut files, mut prefix, mut queues, mut allocation, mut limits) =
        (None, None, None, None, None, None);
    while let Some(field) = fields.field(&mut json)? {
        match field.as_str() {
            "evidence" => put(&mut evidence, digest(&json.string()?)?)?,
            "artifacts" => put(&mut files, artifacts(&mut json)?)?,
            "launch" => put(&mut prefix, launch(&mut json)?)?,
            "max_queue_families" => put(&mut queues, narrow(json.number()?)?)?,
            "max_allocation_bytes" => put(&mut allocation, narrow(json.number()?)?)?,
            "bounds" => put(&mut limits, bounds(&mut json)?)?,
            _ => return Err(invalid("observer JSON input field is unknown")),
        }
    }
    json.space();
    if !json.rest.is_empty() {
        return Err(invalid("observer JSON has trailing bytes"));
    }
    Ok(GpuObserverInput {
        evidence: required(evidence)?,
        artifacts: required(files)?,
        launch: required(prefix)?,
        max_queue_families: required(queues)?,
        max_allocation_bytes: required(allocation)?,
        bounds: required(limits)?,
    })
}

#[cfg(test)]
mod tests;
