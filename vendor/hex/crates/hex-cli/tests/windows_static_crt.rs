//! The `hex.exe` Hex ships loads inside a Windows `oci` box, which maps it in as its launcher, so it
//! imports only DLLs nanoserver carries. nanoserver has `ucrtbase.dll` but no Visual C++ runtime
//! (jupiter, 2026-11-06: an `hex.exe` importing `VCRUNTIME140.dll` couldn't load in the box, so the
//! launcher ended without replying), and no `propsys.dll` (jupiter, 2026-11-11: an `hex.exe` importing it
//! through WMI's VARIANT conversions stopped loading with 0xC0000135, STATUS_DLL_NOT_FOUND, the same
//! silent launcher end). The repository's `.cargo/config.toml` builds Windows targets with `+crt-static`;
//! this reads the built binary's PE import table and refuses any DLL outside the set nanoserver carries
//! (native plan step 4, AC-T7.1.1, AC-T7.4.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

/// Each DLL `hex.exe` imports at load time, every one carried by nanoserver
/// (mcr.microsoft.com/windows/nanoserver:ltsc2025, 10.0.26100.33822, base layer sha256:d828b5f6...).
/// A new import joins only once that image is shown to carry it and every symbol taken from it.
const NANOSERVER_CARRIES: &[&str] = &[
    // In System32.
    "bcryptprimitives.dll",
    "cfgmgr32.dll",
    "combase.dll",
    "mswsock.dll",
    "ntdll.dll",
    "oleaut32.dll",
    // DeleteAppContainerProfile alone, from vendored shards-ipc's AppContainer (Shards D141). userenv.dll
    // exports it at RVA 0xBF10, no forwarder, and each of its load-time imports resolves, in this layer and in
    // nanoserver:ltsc2025 10.0.26100.33674's (diff_id sha256:9a4c0752...); cldapi.dll is a delay-load import.
    "userenv.dll",
    "ws2_32.dll",
    // Reverse forwarders, in System32\forwarders.
    "advapi32.dll",
    "kernel32.dll",
    "ole32.dll",
    "shell32.dll",
    "version.dll",
    // API sets.
    "api-ms-win-core-handle-l1-1-0.dll",
    "api-ms-win-core-synch-l1-2-0.dll",
];

/// The imports in `names` nanoserver doesn't carry.
fn outside_nanoserver(names: &[String]) -> Vec<&str> {
    names
        .iter()
        .map(String::as_str)
        .filter(|n| !NANOSERVER_CARRIES.iter().any(|c| n.eq_ignore_ascii_case(c)))
        .collect()
}

/// AC-T7.4.1: do: check imports naming propsys.dll and VCRUNTIME140.dll beside kernel32.dll and an API
/// set nanoserver carries; expect exactly the two nanoserver lacks refused.
#[test]
fn an_import_nanoserver_lacks_is_refused() {
    let names = [
        "KERNEL32.dll",
        "propsys.dll",
        "api-ms-win-core-synch-l1-2-0.dll",
        "VCRUNTIME140.dll",
    ]
    .map(str::to_owned);
    assert_eq!(
        outside_nanoserver(&names),
        ["propsys.dll", "VCRUNTIME140.dll"]
    );
}

#[cfg(target_os = "windows")]
fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

#[cfg(target_os = "windows")]
fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// The DLL names in PE image `b`'s import directory (PE/COFF: the optional header's import data
/// directory, its descriptors ending at an all-zero one, each `Name` an RVA mapped through the sections).
#[cfg(target_os = "windows")]
fn imports(b: &[u8]) -> Vec<String> {
    let pe = u32_at(b, 0x3c);
    assert_eq!(&b[pe..pe + 4], b"PE\0\0", "not a PE image");
    let coff = pe + 4;
    let sections = u16_at(b, coff + 2);
    let optional = coff + 20;
    let optional_size = u16_at(b, coff + 16);
    assert_eq!(u16_at(b, optional), 0x20b, "not a PE32+ image");
    // PE32+: the data directories start at offset 112; the import directory is the second.
    let import_rva = u32_at(b, optional + 112 + 8);
    let table = optional + optional_size;
    let offset = |rva: usize| -> usize {
        (0..sections)
            .map(|i| table + i * 40)
            .find_map(|s| {
                let (va, size, raw) = (u32_at(b, s + 12), u32_at(b, s + 8), u32_at(b, s + 20));
                (rva >= va && rva < va + size).then(|| rva - va + raw)
            })
            .expect("an RVA outside every section")
    };
    let mut names = Vec::new();
    let mut d = offset(import_rva);
    while u32_at(b, d + 12) != 0 {
        let at = offset(u32_at(b, d + 12));
        let end = b[at..].iter().position(|&c| c == 0).unwrap();
        names.push(String::from_utf8_lossy(&b[at..at + end]).into_owned());
        d += 20;
    }
    names
}

/// Do: read the built `hex.exe`'s imports. Expect: kernel32 among them (the parser reads the table), and no
/// `VCRUNTIME*.dll` or `MSVCP*.dll`.
#[cfg(target_os = "windows")]
#[test]
fn hex_exe_imports_no_visual_cpp_runtime() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_hex")).unwrap();
    let names = imports(&exe);
    assert!(
        names.iter().any(|n| n.eq_ignore_ascii_case("kernel32.dll")),
        "{names:?}"
    );
    let dynamic: Vec<&String> = names
        .iter()
        .filter(|n| {
            let n = n.to_ascii_lowercase();
            n.starts_with("vcruntime") || n.starts_with("msvcp")
        })
        .collect();
    assert!(
        dynamic.is_empty(),
        "hex.exe imports {dynamic:?}, which nanoserver doesn't have"
    );
}

/// AC-T7.4.1: do: read the built `hex.exe`'s imports. Expect: each one a DLL nanoserver carries, so the
/// launcher an `oci` box maps in loads there.
#[cfg(target_os = "windows")]
#[test]
fn hex_exe_imports_only_what_nanoserver_carries() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_hex")).unwrap();
    let names = imports(&exe);
    let lacking = outside_nanoserver(&names);
    assert!(
        lacking.is_empty(),
        "hex.exe imports {lacking:?}, which nanoserver doesn't carry (all: {names:?})"
    );
}
