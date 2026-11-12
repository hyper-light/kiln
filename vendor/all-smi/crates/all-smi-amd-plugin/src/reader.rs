// Copyright 2025 Lablup Inc. and Jeongkyu Shin
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Changed by Hex (github.com/hyper-light/hex): only the kernel's `gpu_busy_percent` reader is kept,
// joined by the VRAM counters beside it, each read from a file kept open; hex-gpu includes this file by
// path (crates/hex-gpu/src/amd.rs). Each change and why is a row of vendor/all-smi/UPSTREAM.md.

//! amdgpu's sysfs counters, each read with one `pread` at offset 0 of a file opened once: sysfs runs
//! the attribute's `show` again for every read from offset 0 (`seq_read_iter`, fs/seq_file.c), so
//! nothing is reopened, sought or walked per sample, and the line is parsed in place.

use std::io;
use std::os::unix::fs::FileExt;

use crate::Unread;

/// The longest line these counters print, a `u64`'s 20 digits and the newline (`sysfs_emit`'s
/// `"%llu\n"`), and one byte more: a line that reaches it is longer than any the kernel prints, refused
/// as malformed rather than cut.
const READ: usize = 22;

/// Read the kernel's `gpu_busy_percent` sysfs attribute: the SMU firmware's percent of load over the
/// GPU's IP cores (drivers/gpu/drm/amd/pm/amdgpu_pm.c).
///
/// amdgpu answers a GPU that is runtime-suspended with `EPERM` rather than wake it
/// (`amdgpu_pm_get_access_if_active`): that GPU is asleep and runs nothing.
pub(crate) fn read_gpu_busy_percent<F: FileExt>(file: &F) -> Result<u8, Unread> {
    match read_u64(file) {
        Ok(value) => parse_gpu_busy_percent(value).ok_or(Unread::Malformed),
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Err(Unread::Asleep),
        Err(error) => Err(unread(&error)),
    }
}

/// A percent past 100 is not one: refused as malformed, not clamped.
fn parse_gpu_busy_percent(value: u64) -> Option<u8> {
    u8::try_from(value).ok().filter(|percent| *percent <= 100)
}

/// Read `mem_info_vram_used` or `mem_info_vram_total`: bytes, no power-management access
/// (drivers/gpu/drm/amd/amdgpu/amdgpu_vram_mgr.c), so a sleeping GPU answers too.
pub(crate) fn read_vram_bytes<F: FileExt>(file: &F) -> Result<u64, Unread> {
    read_u64(file).map_err(|error| unread(&error))
}

/// One read of the counter's line, parsed in place. An `io::Error` of kind `InvalidData` is a line
/// that isn't a number.
fn read_u64<F: FileExt>(file: &F) -> io::Result<u64> {
    let mut line = [0; READ];
    let read = file.read_at(&mut line, 0)?;
    line.get(..read)
        .and_then(parse_line)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
}

/// Decimal digits and one newline, nothing else; `None` for anything more, less or past `u64`.
fn parse_line(line: &[u8]) -> Option<u64> {
    let (newline, digits) = line.split_last()?;
    if *newline != b'\n' || digits.is_empty() || line.len() >= READ {
        return None;
    }
    digits.iter().try_fold(0u64, |value, digit| {
        let digit = u64::from(digit.checked_sub(b'0').filter(|d| *d <= 9)?);
        value.checked_mul(10)?.checked_add(digit)
    })
}

/// A counter's read error: `ENODEV` from a GPU that was removed, as kernfs answers an open file whose
/// node is gone (fs/kernfs/file.c); a line that isn't a number; or the OS's error.
fn unread(error: &io::Error) -> Unread {
    match error.raw_os_error() {
        Some(libc::ENODEV) => Unread::Gone,
        Some(errno) => Unread::Os { errno },
        None if error.kind() == io::ErrorKind::InvalidData => Unread::Malformed,
        None => Unread::Os { errno: 0 },
    }
}
