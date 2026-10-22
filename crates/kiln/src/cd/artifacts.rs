//! Files CD deploys that CI built (docs/cd-targets.md §7): a workflow's artifacts, read from the
//! content store by their SHA-256, and the zips the toolkit uploads them as, opened.
//!
//! Providers read them through [`Files`], which the CD service implements over the store and
//! tests over memory.

use std::io::Read as _;
use std::pin::Pin;

use anyhow::{Context as _, Result, bail, ensure};
use bytes::Bytes;

use crate::hub::Hub;

/// The most an artifact may hold to be read whole.
pub const LIMIT: u64 = 512 << 20;

/// Where a provider reads artifacts' content.
pub trait Files: Send + Sync {
    /// The content of the artifact whose SHA-256 is `hex`, streamed, with its size.
    fn open<'a>(&'a self, hex: &'a str) -> Pin<Box<dyn std::future::Future<Output = Result<(async_channel::Receiver<Bytes>, u64)>> + Send + 'a>>;
}

impl Files for Hub {
    fn open<'a>(&'a self, hex: &'a str) -> Pin<Box<dyn std::future::Future<Output = Result<(async_channel::Receiver<Bytes>, u64)>> + Send + 'a>> {
        Box::pin(async move {
            let obj = crate::ci::services::cas::object(self, hex).await?.with_context(|| format!("no artifact sha256:{hex} in kiln's store (expired?)"))?;
            let size = obj.size;
            Ok((crate::ci::services::cas::read(self, &obj, 0, size), size))
        })
    }
}

/// An artifact's whole content, checked against its hash.
pub async fn read(files: &dyn Files, hex: &str) -> Result<Vec<u8>> {
    let (rx, size) = files.open(hex).await?;
    ensure!(size <= LIMIT, "artifact sha256:{hex} is {size} bytes, more than kiln reads whole ({LIMIT})");
    let mut out = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    while let Ok(b) = rx.recv().await {
        out.extend_from_slice(&b);
    }
    ensure!(out.len() as u64 == size, "artifact sha256:{hex} ended short: {} of {size} bytes", out.len());
    ensure!(crate::util::sha256_hex(&out) == hex, "artifact sha256:{hex} doesn't hash to its name");
    Ok(out)
}

/// A file in a zip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its path in the zip, `/`-separated, never absolute nor leaving the zip.
    pub name: String,
    pub data: Vec<u8>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn as_usize(n: u64) -> Result<usize> {
    usize::try_from(n).context("a zip offset past what this machine addresses")
}

/// The files of a zip (stored or deflated, ZIP64 included); directories skipped. `limit` bounds
/// what they hold once unpacked, so a zip bomb stops early.
pub fn unzip(z: &[u8], limit: u64) -> Result<Vec<Entry>> {
    // The end of central directory record: the last one, within its comment's reach of the end.
    let from = z.len().saturating_sub(22 + 0xffff);
    let eocd = (from..=z.len().saturating_sub(22)).rev().find(|&i| u32_at(z, i) == Some(0x0605_4b50)).context("not a zip (no end of central directory)")?;
    let mut count = u64::from(u16_at(z, eocd + 10).context("a short zip")?);
    let mut dir = u64::from(u32_at(z, eocd + 16).context("a short zip")?);
    // ZIP64: the locator just before the record points at the 64-bit record.
    if (count == 0xffff || dir == 0xffff_ffff) && eocd >= 20 && u32_at(z, eocd - 20) == Some(0x0706_4b50) {
        let at = as_usize(u64_at(z, eocd - 20 + 8).context("a short zip")?)?;
        ensure!(u32_at(z, at) == Some(0x0606_4b50), "a broken ZIP64 record");
        count = u64_at(z, at + 32).context("a short zip")?;
        dir = u64_at(z, at + 48).context("a short zip")?;
    }
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut at = as_usize(dir)?;
    for _ in 0..count {
        ensure!(u32_at(z, at) == Some(0x0201_4b50), "a broken central directory");
        let method = u16_at(z, at + 10).context("a short zip")?;
        let mut packed = u64::from(u32_at(z, at + 20).context("a short zip")?);
        let mut size = u64::from(u32_at(z, at + 24).context("a short zip")?);
        let (n, e, c) = (usize::from(u16_at(z, at + 28).context("a short zip")?), usize::from(u16_at(z, at + 30).context("a short zip")?), usize::from(u16_at(z, at + 32).context("a short zip")?));
        let mut local = u64::from(u32_at(z, at + 42).context("a short zip")?);
        let name = String::from_utf8_lossy(z.get(at + 46..at + 46 + n).context("a short zip")?).replace('\\', "/");
        // ZIP64 sizes and offset, in the order the classic fields overflowed.
        let extra = z.get(at + 46 + n..at + 46 + n + e).context("a short zip")?;
        let mut x = 0usize;
        while x + 4 <= extra.len() {
            let (id, len) = (u16_at(extra, x).unwrap_or(0), usize::from(u16_at(extra, x + 2).unwrap_or(0)));
            if id == 0x0001 {
                let mut f = x + 4;
                for v in [&mut size, &mut packed, &mut local] {
                    if *v == 0xffff_ffff {
                        *v = u64_at(extra, f).context("a broken ZIP64 field")?;
                        f += 8;
                    }
                }
            }
            x += 4 + len;
        }
        at += 46 + n + e + c;
        if name.ends_with('/') {
            continue;
        }
        ensure!(!name.starts_with('/') && !name.split('/').any(|p| p == ".."), "the zip holds {name}, outside itself");
        total = total.saturating_add(size);
        ensure!(total <= limit, "the zip unpacks to more than {limit} bytes");
        let l = as_usize(local)?;
        ensure!(u32_at(z, l) == Some(0x0403_4b50), "a broken local header for {name}");
        let start = l + 30 + usize::from(u16_at(z, l + 26).context("a short zip")?) + usize::from(u16_at(z, l + 28).context("a short zip")?);
        let raw = z.get(start..start + as_usize(packed)?).with_context(|| format!("{name} runs past the zip's end"))?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut d = Vec::with_capacity(as_usize(size.min(limit))?);
                flate2::read::DeflateDecoder::new(raw).take(size + 1).read_to_end(&mut d).with_context(|| format!("inflating {name}"))?;
                d
            }
            m => bail!("{name} is compressed with method {m}; kiln reads stored and deflated files"),
        };
        ensure!(data.len() as u64 == size, "{name} unpacks to {} bytes, not {size}", data.len());
        out.push(Entry { name, data });
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A zip as the toolkit writes one: deflated entries, a directory, a central directory.
    pub fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut z = Vec::new();
        let mut dir = Vec::new();
        let mut n = 0u16;
        for (name, data) in files {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = enc.write_all(data);
            let packed = enc.finish().unwrap_or_default();
            let crc = crc32(data);
            let off = z.len() as u32;
            for (out, central) in [(&mut z, false), (&mut dir, true)] {
                out.extend_from_slice(&(if central { 0x0201_4b50u32 } else { 0x0403_4b50 }).to_le_bytes());
                if central {
                    out.extend_from_slice(&20u16.to_le_bytes());
                }
                out.extend_from_slice(&20u16.to_le_bytes());
                out.extend_from_slice(&0u16.to_le_bytes());
                out.extend_from_slice(&8u16.to_le_bytes());
                out.extend_from_slice(&[0, 0, 0, 0]);
                out.extend_from_slice(&crc.to_le_bytes());
                out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(&(name.len() as u16).to_le_bytes());
                out.extend_from_slice(&0u16.to_le_bytes());
                if central {
                    // Comment length, disk, internal and external attributes; then the offset.
                    out.extend_from_slice(&[0; 10]);
                    out.extend_from_slice(&off.to_le_bytes());
                }
                out.extend_from_slice(name.as_bytes());
                if !central {
                    out.extend_from_slice(&packed);
                }
            }
            n += 1;
        }
        let at = z.len() as u32;
        z.extend_from_slice(&dir);
        z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        z.extend_from_slice(&[0, 0, 0, 0]);
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(dir.len() as u32).to_le_bytes());
        z.extend_from_slice(&at.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for b in data {
            c ^= u32::from(*b);
            for _ in 0..8 {
                c = if c & 1 == 1 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    /// CP-T10.1: what CD deploys is read from kiln's store of bytes, by its SHA-256, checked
    /// against it; content the store doesn't hold is an error naming it.
    #[test]
    fn an_artifact_is_read_from_the_store_by_its_hash() -> Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let dir = tempfile::tempdir()?;
            let z = zip(&[("index.js", b"export default {}"), ("big.bin", &kiln_seams::blobs::noise(3 << 20, 8))]);
            let path = dir.path().join("artifact.zip");
            std::fs::write(&path, &z)?;
            let obj = crate::ci::services::cas::put_file(&hub, &path).await?;
            let got = read(hub.as_ref(), &obj.digest).await?;
            assert!(got == z, "{} bytes read of {}", got.len(), z.len());
            assert_eq!(unzip(&got, 16 << 20)?.len(), 2);
            let missing = read(hub.as_ref(), &crate::util::sha256_hex(b"nothing kept")).await;
            assert!(missing.is_err_and(|e| e.to_string().contains("no artifact")));
            Ok(())
        })
    }

    #[test]
    fn zips_open() -> Result<()> {
        let big = vec![b'a'; 100_000];
        let z = zip(&[("index.js", b"export default {}"), ("assets/app.css", b"body{}"), ("blob.bin", &big)]);
        let files = unzip(&z, 1 << 20)?;
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["index.js", "assets/app.css", "blob.bin"]);
        assert_eq!(files.first().map(|f| f.data.as_slice()), Some(&b"export default {}"[..]));
        assert_eq!(files.get(2).map(|f| f.data.len()), Some(100_000));
        // What unpacks past the limit stops; so does what isn't a zip, or leaves it.
        assert!(unzip(&z, 50_000).is_err());
        assert!(unzip(b"not a zip at all, not even close to one", 1 << 20).is_err());
        assert!(unzip(&zip(&[("../escape", b"x")]), 1 << 20).is_err());
        Ok(())
    }
}
