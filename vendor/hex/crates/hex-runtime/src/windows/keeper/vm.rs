//! The existing keeper handle receipt, committed to one canonical VM incarnation/workload.
use std::io::Write;
use std::path::{Path, PathBuf};

use hex_api::Refusal;
use hex_core::record::{NativeScope, OwnerRef};

const HEADER: &str = "hex-vm-held 1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    pub scope: NativeScope,
    pub commitment: [u8; 32],
    pub pid: u32,
    pub started: u64,
    pub value: usize,
}

fn invalid(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return allocation(message.len());
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "native VM keeper receipt",
        why,
    }
}
fn allocation(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "native VM keeper receipt bytes",
        bound,
    }
}

#[derive(Default)]
struct Length(usize);
impl std::fmt::Write for Length {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        self.0 = self.0.checked_add(value.len()).ok_or(std::fmt::Error)?;
        Ok(())
    }
}
fn hex(out: &mut impl std::fmt::Write, bytes: &[u8]) -> std::fmt::Result {
    for byte in bytes {
        write!(out, "{byte:02x}")?;
    }
    Ok(())
}
impl Held {
    pub(crate) fn commits(&self, expected: Option<(NativeScope, [u8; 32])>) -> bool {
        expected == Some((self.scope, self.commitment))
    }

    fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        writeln!(out, "{HEADER}")?;
        out.write_str("scope ")?;
        hex(out, self.scope.bytes())?;
        out.write_str("\ncommitment ")?;
        hex(out, &self.commitment)?;
        write!(
            out,
            "\nkeeper {} {} {}\n",
            self.pid, self.started, self.value
        )
    }
    pub(crate) fn encode(&self) -> Result<String, Refusal> {
        if self.pid == 0 || self.started == 0 || self.value == 0 {
            return Err(invalid("a native keeper identity/handle is empty"));
        }
        let mut length = Length::default();
        self.render(&mut length)
            .map_err(|_| allocation(usize::MAX))?;
        let mut text = String::new();
        text.try_reserve_exact(length.0)
            .map_err(|_| allocation(length.0))?;
        self.render(&mut text).map_err(|_| allocation(length.0))?;
        Ok(text)
    }
}
fn bound() -> Result<usize, Refusal> {
    // The rendered maximum is derived from exact native integer widths and fixed 128/256-bit fields.
    let maximum = Held {
        scope: NativeScope::from_bytes([u8::MAX; 16]),
        commitment: [u8::MAX; 32],
        pid: u32::MAX,
        started: u64::MAX,
        value: usize::MAX,
    };
    let mut length = Length::default();
    maximum
        .render(&mut length)
        .map_err(|_| allocation(usize::MAX))?;
    Ok(length.0)
}
fn field<'a>(line: Option<&'a str>, name: &str) -> Result<&'a str, Refusal> {
    line.and_then(|line| line.strip_prefix(name))
        .ok_or_else(|| invalid("a keeper field is absent or out of order"))
}
fn unhex<const N: usize>(value: &str) -> Result<[u8; N], Refusal> {
    if Some(value.len()) != N.checked_mul(2) {
        return Err(invalid("a commitment has the wrong width"));
    }
    let mut bytes = [0; N];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid("a commitment has incomplete hex"));
    }
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        let text =
            std::str::from_utf8(pair).map_err(|_| invalid("a commitment is not ASCII hex"))?;
        *byte =
            u8::from_str_radix(text, 16).map_err(|_| invalid("a commitment is not ASCII hex"))?;
    }
    Ok(bytes)
}
pub(crate) fn parse(text: &str) -> Result<Held, Refusal> {
    if text.len() > bound()? || !text.ends_with('\n') {
        return Err(invalid("a keeper receipt is oversized or truncated"));
    }
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(invalid(
            "a keeper receipt has a different format; legacy data supplies no VM authority",
        ));
    }
    let scope = NativeScope::from_bytes(unhex(field(lines.next(), "scope ")?)?);
    let commitment = unhex(field(lines.next(), "commitment ")?)?;
    let mut keeper = field(lines.next(), "keeper ")?.split(' ');
    let pid = keeper
        .next()
        .ok_or_else(|| invalid("keeper PID is absent"))?
        .parse::<u32>()
        .map_err(|_| invalid("keeper PID is invalid"))?;
    let started = keeper
        .next()
        .ok_or_else(|| invalid("keeper birth is absent"))?
        .parse::<u64>()
        .map_err(|_| invalid("keeper birth is invalid"))?;
    let value = keeper
        .next()
        .ok_or_else(|| invalid("keeper handle is absent"))?
        .parse::<usize>()
        .map_err(|_| invalid("keeper handle is invalid"))?;
    if pid == 0 || started == 0 || value == 0 || keeper.next().is_some() || lines.next().is_some() {
        return Err(invalid(
            "keeper receipt contains empty identities or extra fields",
        ));
    }
    Ok(Held {
        scope,
        commitment,
        pid,
        started,
        value,
    })
}

pub(crate) fn read(path: &Path) -> Result<Option<Held>, Refusal> {
    let Some(bytes) = super::read_native_bytes(path, bound()?)? else {
        return Ok(None);
    };
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("VM keeper receipt is not UTF-8"))?;
    parse(text).map(Some)
}

pub(crate) fn pool_from_name(name: &str) -> Result<[u8; 32], Refusal> {
    unhex(
        name.strip_prefix("pool-held-")
            .ok_or_else(|| invalid("a backing keeper receipt has a different name"))?,
    )
}

pub(crate) fn path(state: &Path, owner: OwnerRef<'_>) -> Result<PathBuf, Refusal> {
    let (prefix, key_size) = match owner {
        OwnerRef::Box(id) => ("held-", id.as_str().len()),
        // Disjoint from legacy held-<BoxId>; the receipt itself binds the full generation.
        OwnerRef::Pool(pool) => (
            "pool-held-",
            pool.len()
                .checked_mul(2)
                .ok_or_else(|| allocation(usize::MAX))?,
        ),
    };
    let name_size = prefix
        .len()
        .checked_add(key_size)
        .ok_or_else(|| allocation(usize::MAX))?;
    let size = state
        .as_os_str()
        .len()
        .checked_add(1)
        .and_then(|size| size.checked_add(name_size))
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut name = String::new();
    name.try_reserve_exact(name_size)
        .map_err(|_| allocation(size))?;
    name.push_str(prefix);
    match owner {
        OwnerRef::Box(id) => name.push_str(id.as_str()),
        OwnerRef::Pool(pool) => hex(&mut name, pool).map_err(|_| allocation(size))?,
    }
    let mut path = PathBuf::new();
    path.try_reserve_exact(size).map_err(|_| allocation(size))?;
    path.push(state);
    path.push(name);
    Ok(path)
}

pub(crate) fn write(path: &Path, held: &Held) -> Result<(), Refusal> {
    let text = held.encode()?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("keeper receipt has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("keeper receipt has no name"))?;
    let name_size = name
        .len()
        .checked_add("..tmp".len())
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut temporary_name = std::ffi::OsString::new();
    temporary_name
        .try_reserve_exact(name_size)
        .map_err(|_| allocation(name_size))?;
    temporary_name.push(".");
    temporary_name.push(name);
    temporary_name.push(".tmp");
    let size = parent
        .as_os_str()
        .len()
        .checked_add(1)
        .and_then(|size| size.checked_add(name_size))
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut temporary = PathBuf::new();
    temporary
        .try_reserve_exact(size)
        .map_err(|_| allocation(size))?;
    temporary.push(parent);
    temporary.push(temporary_name);
    let mut file = std::fs::File::create(&temporary)
        .map_err(|error| Refusal::os("creating the VM keeper receipt temp", error))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| Refusal::os("flushing the VM keeper receipt", error))?;
    drop(file);
    super::super::sys::replace_durable(&temporary, path)
        .map_err(|error| Refusal::os("publishing the VM keeper receipt", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt() -> Held {
        Held {
            scope: NativeScope::from_bytes([1; 16]),
            commitment: [2; 32],
            pid: 7,
            started: 11,
            value: 13,
        }
    }
    /// AC-T7.4.1: the SAME keeper namespace separates an actual pool from every Box string.
    #[test]
    fn pool_receipt_path_cannot_alias_a_legacy_box_receipt() {
        let dir = Path::new("state");
        let pool = [3; 32];
        let box_id = hex_api::BoxId::new("pool-held-0303").unwrap();
        let legacy = path(dir, OwnerRef::Box(&box_id)).unwrap();
        let backing = path(dir, OwnerRef::Pool(&pool)).unwrap();
        assert_ne!(legacy, backing);
        assert_eq!(legacy, dir.join("held-pool-held-0303"));
        assert_eq!(backing.file_name().unwrap().len(), "pool-held-".len() + 64);
    }

    #[test]
    fn scope_spec_and_exact_keeper_identity_roundtrip() {
        let r = receipt();
        assert_eq!(parse(&r.encode().unwrap()).unwrap(), r);
    }
    #[test]
    fn truncated_unknown_legacy_or_extra_data_is_never_authority() {
        let text = receipt().encode().unwrap();
        for cut in 0..text.len() {
            assert!(parse(text.get(..cut).unwrap()).is_err());
        }
        for bad in [
            "7 11 13".to_owned(),
            text.replace("held 1", "held 2"),
            format!("{text}extra\n"),
            text.replace("keeper 7 11 13", "keeper 0 11 13"),
        ] {
            assert!(parse(&bad).is_err());
        }
    }

    /// AC-T7.4.1: a same-ID replacement cannot borrow the previous scope's cleanup authority.
    #[test]
    fn another_scope_or_workload_never_inherits_a_keeper_receipt() {
        let held = receipt();
        assert!(held.commits(Some((held.scope, held.commitment))));
        assert!(!held.commits(None));
        assert!(!held.commits(Some((NativeScope::from_bytes([3; 16]), held.commitment))));
        assert!(!held.commits(Some((held.scope, [4; 32]))));
        let retained = parse(&held.encode().unwrap()).unwrap();
        assert!(!retained.commits(Some((NativeScope::from_bytes([3; 16]), [4; 32]))));
    }
}
