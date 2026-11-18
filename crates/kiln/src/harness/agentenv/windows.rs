//! The variables Windows keeps for its users in the registry, read as they stand now: the system's
//! (`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`) and the user's
//! (`HKCU\Environment`), each value a `REG_SZ` or a `REG_EXPAND_SZ` (Microsoft Learn, "Environment
//! Variables", learn.microsoft.com/windows/win32/procthread/environment-variables). Declared by hand
//! as `crate::os::win32` declares its calls (std only, so it type-checks for Windows from any host).

#![allow(unsafe_code)]

use std::io;

use super::places::RegValue;

const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as i32 as isize;
const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const KEY_QUERY_VALUE: u32 = 0x0001;
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_MORE_DATA: i32 = 234;
const ERROR_NO_MORE_ITEMS: i32 = 259;
/// The longest a value's name may be, in UTF-16 units with its NUL (Microsoft Learn, "Registry
/// element size limits": 16,383 characters).
const NAME_UNITS: usize = 16_384;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(key: isize, subkey: *const u16, options: u32, access: u32, result: *mut isize) -> i32;
    fn RegEnumValueW(key: isize, index: u32, name: *mut u16, name_len: *mut u32, reserved: *mut u32, kind: *mut u32, data: *mut u8, data_len: *mut u32) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

struct Key(isize);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key this value opened.
        unsafe { RegCloseKey(self.0) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to its first NUL.
fn text(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(units.get(..end).unwrap_or_default())
}

/// The `REG_SZ` and `REG_EXPAND_SZ` values of `root\subkey`, in the key's order, `REG_EXPAND_SZ`
/// ones as stored (`%VARIABLES%` not expanded); none when the key isn't there.
fn values(root: isize, subkey: &str) -> io::Result<Vec<RegValue>> {
    let sub = wide(subkey);
    let mut raw = 0isize;
    // SAFETY: a predefined root key, a NUL-terminated subkey and a place for the result.
    let rc = unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut raw) };
    match rc {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND => return Ok(Vec::new()),
        _ => return Err(io::Error::from_raw_os_error(rc)),
    }
    let key = Key(raw);
    let mut out = Vec::new();
    let mut name: Vec<u16> = vec![0; NAME_UNITS];
    let mut data: Vec<u16> = vec![0; 2048];
    let mut index = 0u32;
    // Each value once; one that grows while it's read is asked again with room for it.
    let mut tries = 0u32;
    loop {
        let mut name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        let mut data_len = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
        let mut kind = 0u32;
        // SAFETY: an open key, buffers of `name_len` units and `data_len` bytes, and places for the rest.
        let rc = unsafe { RegEnumValueW(key.0, index, name.as_mut_ptr(), &mut name_len, std::ptr::null_mut(), &mut kind, data.as_mut_ptr().cast(), &mut data_len) };
        match rc {
            ERROR_SUCCESS => {
                tries = 0;
                index = index.saturating_add(1);
                if kind != REG_SZ && kind != REG_EXPAND_SZ {
                    continue;
                }
                let units = usize::try_from(data_len).unwrap_or(0) / 2;
                let name = text(name.get(..usize::try_from(name_len).unwrap_or(0)).unwrap_or_default());
                out.push(RegValue { name, data: text(data.get(..units.min(data.len())).unwrap_or_default()), expand: kind == REG_EXPAND_SZ });
            }
            ERROR_NO_MORE_ITEMS => return Ok(out),
            ERROR_MORE_DATA if tries < 8 => {
                tries += 1;
                data.resize(usize::try_from(data_len).unwrap_or(0) / 2 + 1, 0);
            }
            _ => return Err(io::Error::from_raw_os_error(rc)),
        }
    }
}

/// The system's variables and the user's, as the registry holds them now.
pub fn registry() -> io::Result<(Vec<RegValue>, Vec<RegValue>)> {
    Ok((values(HKEY_LOCAL_MACHINE, "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment")?, values(HKEY_CURRENT_USER, "Environment")?))
}
