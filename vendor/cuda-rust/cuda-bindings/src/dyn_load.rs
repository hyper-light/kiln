// SPDX-FileCopyrightText: Copyright (c) 2021-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Changed by Hex (github.com/hyper-light/hex): this loads NVML, the NVIDIA Management Library, in place
// of the CUDA driver and cuRAND, for hex-gpu (crates/hex-gpu/src/nvml.rs includes it by path). Each
// change and why is a row of vendor/cuda-rust/UPSTREAM.md.

//! Runtime loading policy for NVML.
//!
//! [`super::sys`] owns the ABI surface and function signatures (NVIDIA's NVML reference, `nvml.h`).
//! This module only chooses library names, keeps the loaded and initialised library for the process,
//! and tells a library that didn't load from one that loaded without a symbol Hex calls or whose
//! initialisation failed.

use std::sync::OnceLock;

use super::sys::Api;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DynLoadError {
    LoadFailed {
        names: &'static [&'static str],
        source: String,
    },
    MissingSymbol {
        name: &'static str,
    },
    InitFailed {
        code: i32,
    },
}

impl std::fmt::Display for DynLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynLoadError::LoadFailed { names, source } => {
                write!(f, "failed to load any of {names:?}: {source}")
            }
            DynLoadError::MissingSymbol { name } => {
                write!(f, "the NVML library loaded has no {name}")
            }
            DynLoadError::InitFailed { code } => {
                write!(f, "nvmlInit_v2 returned {code}")
            }
        }
    }
}

impl std::error::Error for DynLoadError {}

// The soname NVIDIA's driver installs, then WSL's copy, which WSL 2 keeps outside the loader's default
// path.
#[cfg(target_os = "linux")]
const NVML_LIB_NAMES: &[&str] = &["libnvidia-ml.so.1", "/usr/lib/wsl/lib/libnvidia-ml.so.1"];
// Searched in System32 alone (super::sys), where NVIDIA's driver installs it.
#[cfg(target_os = "windows")]
const NVML_LIB_NAMES: &[&str] = &["nvml.dll"];

/// The first of `names` that `open` loads. A library that loaded but lacks a symbol is the right
/// library broken, so it stops the search; one that didn't load moves it to the next name.
pub(crate) fn load_api<T>(
    names: &'static [&'static str],
    mut open: impl FnMut(&'static str) -> Result<T, DynLoadError>,
) -> Result<T, DynLoadError> {
    let mut last_error = String::new();
    for &name in names {
        match open(name) {
            Ok(api) => return Ok(api),
            Err(DynLoadError::LoadFailed { source, .. }) => last_error = source,
            Err(other) => return Err(other),
        }
    }

    Err(DynLoadError::LoadFailed {
        names,
        source: last_error,
    })
}

/// Set once NVML has loaded and initialised; never closed or shut down, so the device handles taken
/// through it stay valid for the process. A failure isn't kept: the next call tries again, so a
/// driver installed or loaded after the node started is found at its next open.
static NVML: OnceLock<Api> = OnceLock::new();

pub(crate) fn nvml() -> Result<&'static Api, DynLoadError> {
    if let Some(api) = NVML.get() {
        return Ok(api);
    }
    let api = load_api(NVML_LIB_NAMES, Api::open)?;
    api.init()
        .map_err(|code| DynLoadError::InitFailed { code })?;
    Ok(NVML.get_or_init(|| api))
}
