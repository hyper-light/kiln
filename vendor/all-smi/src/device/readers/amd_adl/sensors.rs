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
// Changed by Hex (github.com/hyper-light/hex): only the GFX activity reading is kept, its answer a
// typed refusal where the table has none to give; hex-gpu includes this file by path
// (crates/hex-gpu/src/adl.rs). Each change and why is a row of vendor/all-smi/UPSTREAM.md.

//! Extracting a reading from an ADL PMLog sensor table.
//!
//! `ADL2_New_QueryPMLogData_Get` fills a 256-entry table indexed by `ADL_PMLOG_SENSORS`, so the
//! sensor whose enum value is `n` is entry `n`, its `supported` flag saying whether the card
//! publishes it. The index is AMD's (`adl_defines.h`).

use super::ffi::AdlPmLogDataOutput;
use crate::Unread;

/// `ADL_PMLOG_INFO_ACTIVITY_GFX`: the graphics engine's activity, in percent.
pub const PMLOG_INFO_ACTIVITY_GFX: usize = 19;

/// A percent past 100 is not one: refused as malformed, not clamped.
const ACTIVITY_PCT: std::ops::RangeInclusive<i32> = 0..=100;

/// The graphics engine's activity in percent: `Unsupported` where the card doesn't publish it,
/// `Malformed` outside 0 to 100.
pub fn gfx_activity(output: &AdlPmLogDataOutput) -> Result<u8, Unread> {
    let entry = output
        .sensors
        .get(PMLOG_INFO_ACTIVITY_GFX)
        .ok_or(Unread::Unsupported)?;
    if entry.supported == 0 {
        return Err(Unread::Unsupported);
    }
    if !ACTIVITY_PCT.contains(&entry.value) {
        return Err(Unread::Malformed);
    }
    u8::try_from(entry.value).map_err(|_| Unread::Malformed)
}
