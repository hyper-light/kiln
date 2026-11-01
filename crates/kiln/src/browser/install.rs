//! The browser kiln runs for previews: Chrome for Testing at a pinned version, downloaded once per
//! machine into kiln's own folder and checked (its size and SHA-256) before it's unpacked. kiln
//! never uses a browser the person installed, nor their profile.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The version every machine runs, so a preview looks the same wherever it's made.
pub const VERSION: &str = "155.0.8059.84";

struct Build {
    platform: &'static str,
    size: u64,
    sha256: &'static str,
    /// The executable, inside the archive's folder.
    exe: &'static str,
}

const BUILDS: [Build; 5] = [
    Build { platform: "linux64", size: 199_014_491, sha256: "cf0f049892dea22e6a6db26d47ed0d7776b7158edcbec4634086633df8bafcbc", exe: "chrome-linux64/chrome" },
    Build { platform: "linux-arm64", size: 198_807_353, sha256: "1692a407908867de88eb1f1b2a93ca47c348d8fb2e3c04752d966e0f2e0344b4", exe: "chrome-linux-arm64/chrome" },
    Build {
        platform: "mac-arm64",
        size: 192_672_771,
        sha256: "13b9e98ce56f5ce18e100a009f455c9285f399e7c9dc8a76fdf2f3a9755bb5f2",
        exe: "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    },
    Build {
        platform: "mac-x64",
        size: 203_150_281,
        sha256: "d805a173118daa5f6f5cf464500789e1e00022a9a6aed8abfec821ab828fa862",
        exe: "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    },
    Build { platform: "win64", size: 206_114_888, sha256: "507f2a031441d10c62b5e3d4e863f09ac1198a571c1419976cbd8387f639afde", exe: "chrome-win64/chrome.exe" },
];

fn build() -> anyhow::Result<&'static Build> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("windows", "x86_64") => "win64",
        (os, arch) => anyhow::bail!("kiln has no preview browser for {os}/{arch}"),
    };
    BUILDS.iter().find(|b| b.platform == platform).ok_or_else(|| anyhow::anyhow!("kiln has no preview browser for {platform}"))
}

/// One install at a time in this process (another process waits on the folder's own lock).
static INSTALLING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// Where the browser's executable is under `dir`, once [`ensure`] put it there.
pub fn exe_in(dir: &Path) -> anyhow::Result<PathBuf> {
    Ok(dir.join("browser").join(VERSION).join(build()?.exe))
}

/// The browser's executable under `dir`, downloading and unpacking it first when it isn't there.
/// The first time on a machine that's a download of about 200 MB.
pub async fn ensure(dir: &Path) -> anyhow::Result<PathBuf> {
    let b = build()?;
    let home = dir.join("browser");
    let root = home.join(VERSION);
    let exe = exe_in(dir)?;
    if exe.exists() {
        return Ok(exe);
    }
    let _one = INSTALLING.lock().await;
    if exe.exists() {
        return Ok(exe);
    }
    let url = format!("https://storage.googleapis.com/chrome-for-testing-public/{VERSION}/{p}/chrome-{p}.zip", p = b.platform);
    let part = home.join(format!("chrome-{}-{VERSION}.zip.part", b.platform));
    tracing::info!(version = VERSION, platform = b.platform, "downloading the preview browser (once on this machine)");
    download(&url, &part, b.size, b.sha256).await?;
    let staging = home.join(format!("{VERSION}.part"));
    let (zip, out, to) = (part.clone(), staging.clone(), root.clone());
    crate::rt::blocking(move || -> anyhow::Result<()> {
        unpack(&zip, &out)?;
        std::fs::rename(&out, &to)?;
        std::fs::remove_file(&zip)?;
        Ok(())
    })
    .await??;
    anyhow::ensure!(exe.exists(), "the preview browser's archive had no {}", b.exe);
    // Older versions kiln used before go.
    if let Ok(entries) = std::fs::read_dir(&home) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && name != VERSION && name != "profiles" && name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Ok(exe)
}

/// `url` into `to`, checked against its pinned size and SHA-256 as it comes.
async fn download(url: &str, to: &Path, size: u64, sha256: &str) -> anyhow::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let client = crate::http::client::Client::public()?;
    let mut resp = client.get(url).timeout(Duration::from_secs(1800)).send().await?;
    anyhow::ensure!(resp.is_success(), "the preview browser's download answered {}", resp.status());
    let path = to.to_path_buf();
    let mut file = crate::rt::blocking(move || std::fs::File::create(path)).await??;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 20);
    loop {
        let chunk = resp.chunk().await?;
        let done = chunk.is_none();
        if let Some(c) = chunk {
            hasher.update(&c);
            got += c.len() as u64;
            anyhow::ensure!(got <= size, "the preview browser's download is bigger than it should be");
            buf.extend_from_slice(&c);
        }
        // Written a megabyte at a time, off the async loop.
        if buf.len() >= 1 << 20 || (done && !buf.is_empty()) {
            let out = std::mem::take(&mut buf);
            file = crate::rt::blocking(move || -> std::io::Result<std::fs::File> {
                file.write_all(&out)?;
                Ok(file)
            })
            .await??;
            buf = Vec::with_capacity(1 << 20);
        }
        if done {
            break;
        }
    }
    crate::rt::blocking(move || file.sync_all()).await??;
    let sum = hex::encode(hasher.finalize());
    if got != size || sum != sha256 {
        let _ = std::fs::remove_file(to);
        anyhow::bail!("the preview browser's download isn't the one kiln expects ({got} bytes, sha256 {sum})");
    }
    Ok(())
}

/// A zip into `dest` (its permissions and links as they are in it: the Mac's app has links).
fn unpack(zip: &Path, dest: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest)?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(zip)?))?;
    archive.extract(dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_platform_kiln_runs_on_has_a_pinned_build() {
        assert!(BUILDS.iter().all(|b| b.sha256.len() == 64 && b.size > 100 << 20 && b.exe.starts_with(&format!("chrome-{}/", b.platform))));
        if matches!((std::env::consts::OS, std::env::consts::ARCH), ("linux" | "macos", "x86_64" | "aarch64")) {
            assert!(build().is_ok());
        }
    }

    #[test]
    fn an_archive_unpacks_with_its_links_and_modes() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-unpack-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let zip_path = dir.join("a.zip");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip_path)?);
            let exec = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            w.start_file("app/bin/run", exec)?;
            w.write_all(b"#!/bin/sh\necho hi\n")?;
            w.add_symlink("app/current", "bin", zip::write::SimpleFileOptions::default())?;
            w.finish()?;
        }
        let out = dir.join("out");
        unpack(&zip_path, &out)?;
        assert_eq!(std::fs::read(out.join("app/bin/run"))?, b"#!/bin/sh\necho hi\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(out.join("app/bin/run"))?.permissions().mode() & 0o777, 0o755);
            assert!(std::fs::symlink_metadata(out.join("app/current"))?.file_type().is_symlink());
            assert!(out.join("app/current/run").exists());
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
