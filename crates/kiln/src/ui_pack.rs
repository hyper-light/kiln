//! What of the UI's files is worth compressing, and how it's compressed: one rule for the build, which packs them
//! ahead into the binary with brotli and with gzip (`build.rs`'s `PACKED` and `PACKED_GZ` tables), and for the
//! server, which packs with brotli what the build didn't (`crate::ui`: a debug build's files changed since it was
//! built). Goal 11, plan 01a1a87b2e56 step 5: a file the build packed is served from the binary's own pages, never
//! compressed again as the server starts nor kept on its heap; goal 11's audit, row B15: a client that takes gzip
//! alone gets the build's gzip copy from its first request, as a browser gets the brotli one.
//!
//! This file is the crate's `ui_pack` module and, by its path, a module of `build.rs`. So it takes only what both
//! have: `std`, and `brotli`, `flate2`, `mime_guess` and `sha2`, which are the crate's dependencies and its
//! build-dependencies alike.

use std::fmt::Write as _;
use std::io::Write as _;

/// Files of this many bytes or fewer go as they are: kiln's line from before the build packed them.
pub const LEAST: usize = 1024;
/// brotli's quality: its best, as the server packed them before.
const QUALITY: u32 = 11;
/// What brotli's writer packs through at a time, as the server's did.
const BUFFER: usize = 4096;

/// Whether a file of `mime` compresses: text, scripts, JSON, SVG and WebAssembly (the syntax grammars and
/// web-tree-sitter's runtime, built as plain WebAssembly for the browser to decode: goal 8 of the browser
/// protocol program, plan 01a1a832ebfd step 3).
pub fn compressible(mime: &str) -> bool {
    mime.starts_with("text/") || mime.contains("javascript") || mime.contains("json") || mime.contains("svg") || mime == "application/wasm"
}

/// The type a file called `name` is served as.
pub fn mime_of(name: &str) -> String {
    mime_guess::from_path(name).first_or_octet_stream().as_ref().to_owned()
}

/// Whether a file of `mime`, `len` bytes long, is worth packing.
pub fn worth(mime: &str, len: usize) -> bool {
    compressible(mime) && len > LEAST
}

/// The brotli window for `len` bytes, as bits: the smallest that holds them all, as `crate::util::brotli_window`
/// says (the build can't reach the crate's modules; a test holds the two alike).
pub fn window_bits(len: usize) -> u32 {
    // A window of `bits` holds (1 << bits) - 16 bytes.
    (10..22).find(|bits| (1usize << bits).saturating_sub(16) >= len).unwrap_or(22)
}

/// `data` packed with brotli at its best, in the window that holds it; none if the writer refused it.
pub fn brotli(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 3);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, BUFFER, QUALITY, window_bits(data.len()));
        w.write_all(data).ok()?;
        w.flush().ok()?;
        // Dropped here, which ends the stream.
    }
    Some(out)
}

/// `data` packed with gzip at its best (level 9, as the server packed a copy on request before), with no name and no
/// time in its header, so the same file packs to the same bytes in every build; none if the writer refused it.
pub fn gzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut w = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 3), flate2::Compression::best());
    w.write_all(data).ok()?;
    w.finish().ok()
}

/// The SHA-256 of `data`: what a file is known by (rust-embed's `sha256_hash` says the same of an embedded
/// file), so that a copy the build packed goes out only for the very file it was packed from.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(data).into()
}

/// The source of one of the build's tables of packed files, the static `name` of the files packed with `coding`,
/// from `entries`: each a file's name, its SHA-256 and the path of its packed bytes, which `include_bytes!` takes. In
/// name order, for a binary search.
pub fn table(name: &str, coding: &str, entries: &[(String, [u8; 32], String)]) -> String {
    let mut sorted: Vec<&(String, [u8; 32], String)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = format!(
        "/// The UI's files the build packed with {coding} (`build.rs`, `crate::ui_pack`): each its name, the SHA-256 of\n\
         /// the file it was packed from and its packed bytes, in name order.\n\
         pub static {name}: &[(&str, [u8; 32], &[u8])] = &[\n",
    );
    for (name, sha, path) in sorted {
        let _ = writeln!(out, "    ({name:?}, {sha:?}, include_bytes!({path:?})),");
    }
    out.push_str("];\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 5: the build packs what the server would: text, scripts, JSON and SVG
    /// past 1 KiB, nothing else; and WebAssembly (goal 8 of the browser protocol program, plan 01a1a832ebfd
    /// step 3).
    #[test]
    fn g11_s5_the_build_and_the_server_take_the_same_files() {
        assert!(worth(&mime_of("assets/index-a1.js"), 2000));
        assert!(worth(&mime_of("assets/index-a1.css"), 2000));
        assert!(worth(&mime_of("_shell.html"), 2000));
        assert!(worth(&mime_of("manifest.webmanifest"), 2000));
        assert!(worth(&mime_of("assets/logo.svg"), 2000));
        assert!(worth(&mime_of("assets/tree-sitter.wasm"), 2000));
        assert!(worth(&mime_of("assets/typescript-AbCdEfGh.wasm"), 2000));
        assert!(!worth(&mime_of("assets/index-a1.js"), LEAST), "1 KiB or less goes as it is");
        for raw in ["assets/geist-latin-wght-normal.woff2", "assets/scala.wasm-B.gz", "icon.png"] {
            assert!(!worth(&mime_of(raw), 1 << 20), "{raw} is packed already, or doesn't compress");
        }
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the build's window is the server's, for every size up to past 4 MiB.
    #[test]
    fn g11_s5_the_builds_window_is_the_servers() {
        for len in (0..(5 << 20)).step_by(4093).chain([0, 1008, 1009, (1 << 22) - 16, (1 << 22) - 15, usize::MAX]) {
            assert_eq!(window_bits(len), crate::util::brotli_window(len), "{len}");
        }
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: what's packed comes back whole, and smaller.
    #[test]
    fn g11_s5_a_packed_file_comes_back_whole() {
        let data: String = (0..20_000).map(|i| format!("export const kiln{i} = {i};\n")).collect();
        let packed = brotli(data.as_bytes()).unwrap_or_default();
        let mut back = Vec::new();
        let _ = ::brotli::BrotliDecompress(&mut packed.as_slice(), &mut back);
        assert_eq!(back, data.as_bytes());
        assert!(packed.len() < data.len() / 10);
    }

    /// Goal 11's audit, row B15: what gzip packs comes back whole and smaller, and the same file packs to the same
    /// bytes every time (its header holds no name and no time), so a build's copy is the file's in every build.
    #[test]
    fn g11_b15_a_gzipped_file_comes_back_whole_and_the_same() {
        use std::io::Read as _;
        let data: String = (0..20_000).map(|i| format!("export const kiln{i} = {i};\n")).collect();
        let packed = gzip(data.as_bytes()).unwrap_or_default();
        let mut back = Vec::new();
        let read = flate2::read::GzDecoder::new(packed.as_slice()).read_to_end(&mut back);
        assert!(read.is_ok(), "{read:?}");
        assert_eq!(back, data.as_bytes());
        assert!(packed.len() < data.len() / 5);
        assert_eq!(gzip(data.as_bytes()), Some(packed), "the same bytes again");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the table is in name order, each entry its name, its file's SHA-256 and
    /// the path its bytes are included from.
    #[test]
    fn g11_s5_the_builds_table_is_in_name_order() {
        let a = sha256(b"a");
        let src = table("PACKED", "brotli", &[("z.js".into(), [7; 32], "/out/z.br".into()), ("assets/a \"b\".js".into(), a, "/out/a.br".into())]);
        let first = src.find("assets/a").unwrap_or(usize::MAX);
        let second = src.find("z.js").unwrap_or(0);
        assert!(first < second, "{src}");
        assert!(src.contains(r#"("assets/a \"b\".js", [202, 151, 129, 18,"#), "{src}");
        assert!(src.contains(r#"include_bytes!("/out/a.br")"#), "{src}");
        assert!(src.starts_with("/// The UI's files the build packed with brotli") && src.contains("pub static PACKED:") && src.trim_end().ends_with("];"), "{src}");
        assert_eq!(sha256(b"a").first(), Some(&0xca), "SHA-256 of \"a\" is ca978112...");
    }
}
