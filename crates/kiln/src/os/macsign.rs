//! A code signature of its own for kiln on a Mac (docs/releases.md). macOS's local network privacy
//! knows a program by its code signature, and kiln's builds are ad hoc signed, each by its linker
//! under a name of its own. To macOS every upgrade is a program it has never seen, cut off from
//! the local network (and the kiln server with it) until someone allows it again. A launcher that
//! stays the same doesn't change that: on 2026-10-18 a fresh build started by an unchanged
//! launcher was refused all the same.
//!
//! `kiln signing setup`, once per Mac, gives kiln a signing identity there. It's a self-signed
//! certificate for code signing, in a keychain of kiln's own, which macOS is asked once to trust
//! for code signing. From then on kiln signs every binary it installs or starts on the Mac with it,
//! under one identifier: the launcher, the installed `kiln`, each upgrade's build. To macOS they're
//! one program, allowed onto the local network once. Without the identity, binaries stay as built.
//!
//! A binary signed here keeps the build it was: beside it, `<binary>.kiln-build` says which, for
//! as long as the binary is the one signed. So the server knows it for what it handed out.

use std::path::{Path, PathBuf};

/// The identifier every kiln binary is signed under.
pub const IDENTIFIER: &str = "io.github.hyper-light.kiln";

/// The build a binary at `path` whose SHA-256 is `sha256` was before it was signed here, when it
/// was (else `None`: it is the build its own bytes are).
pub fn build_of(path: &Path, sha256: &str) -> Option<String> {
    let raw = std::fs::read(sidecar(path)).ok()?;
    let s: Signed = serde_json::from_slice(&raw).ok()?;
    s.sha256.eq_ignore_ascii_case(sha256).then_some(s.build)
}

/// What kiln signing is on this machine: ready (its identity), not set up (why), or nothing a
/// machine other than a Mac needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Ready(String),
    Missing(String),
    NotNeeded,
}

pub fn state() -> State {
    imp::state()
}

/// Sign `tmp`, a copy not yet in place, which becomes `dest`, as build `build`. `Ok(false)`: this
/// machine has no identity to sign with, so it stays as built.
pub fn sign_as(tmp: &Path, dest: &Path, build: &str) -> anyhow::Result<bool> {
    imp::sign_as(tmp, dest, build)
}

/// `kiln signing setup`: the identity (made, and trusted after macOS asks once), then every kiln
/// binary installed here signed with it. What was signed.
pub fn setup() -> anyhow::Result<Vec<PathBuf>> {
    imp::setup()
}

/// Sign the kiln binary at `path`, in place by a copy renamed over it (a process running it keeps
/// the old one), as the build it is. `Ok(false)`: no identity.
pub fn resign(path: &Path) -> anyhow::Result<bool> {
    let (sha256, _) = crate::updates::hash_file(path)?;
    let build = build_of(path, &sha256).unwrap_or_else(|| crate::updates::build_id(&sha256));
    let tmp = path.with_extension(format!("signing.{}", std::process::id()));
    std::fs::copy(path, &tmp)?;
    match sign_as(&tmp, path, &build) {
        Ok(true) => {
            std::fs::rename(&tmp, path)?;
            Ok(true)
        }
        other => {
            let _ = std::fs::remove_file(&tmp);
            other
        }
    }
}

/// `kiln signing [status|setup]`.
#[derive(clap::Args)]
pub struct SigningArgs {
    #[command(subcommand)]
    cmd: Option<SigningCmd>,
}

#[derive(clap::Subcommand)]
enum SigningCmd {
    /// Whether kiln signs its builds on this machine (the default).
    Status,
    /// Once per Mac: make kiln's signing identity (macOS asks once to trust it for code signing)
    /// and sign every kiln binary installed here with it. From then on each upgrade is signed with
    /// it, so macOS lets kiln onto the local network once, not after every upgrade.
    Setup,
}

pub fn run(a: SigningArgs) -> anyhow::Result<()> {
    match a.cmd.unwrap_or(SigningCmd::Status) {
        SigningCmd::Status => {
            match state() {
                State::Ready(id) => println!("kiln signs its builds on this Mac (identity {id}, identifier {IDENTIFIER}): macOS knows every upgrade as one program"),
                State::Missing(why) => println!("{why}"),
                State::NotNeeded => println!("nothing to set up: only a Mac needs kiln's own signature"),
            }
            Ok(())
        }
        SigningCmd::Setup => {
            let signed = setup()?;
            println!("kiln signs its builds on this Mac now; signed {} binar{} installed here:", signed.len(), if signed.len() == 1 { "y" } else { "ies" });
            for b in &signed {
                println!("  {}", b.display());
            }
            println!("The runner takes up its signed binary in a moment. macOS may ask once whether kiln may find devices on your local network: allow it. Upgrades keep that from now on.");
            Ok(())
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Signed {
    build: String,
    /// The binary as signed.
    sha256: String,
}

fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    name.push(".kiln-build");
    path.with_file_name(name)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn remember(dest: &Path, signed: &Path, build: &str) -> anyhow::Result<()> {
    let (sha256, _) = crate::updates::hash_file(signed)?;
    let body = serde_json::to_vec(&Signed { build: build.to_owned(), sha256 })?;
    crate::util::write_secret(&sidecar(dest), &body, 0o644)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Path, PathBuf, State};

    pub fn state() -> State {
        State::NotNeeded
    }

    pub fn sign_as(_tmp: &Path, _dest: &Path, _build: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    pub fn setup() -> anyhow::Result<Vec<PathBuf>> {
        anyhow::bail!("only a Mac needs kiln's own signature: elsewhere upgrades keep their permissions as they are")
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::{Command, Stdio};

    use anyhow::Context as _;

    use super::{IDENTIFIER, Path, PathBuf, State};

    const NAME: &str = "kiln code signing";

    fn dir() -> PathBuf {
        crate::config::default_data_dir().join("signing")
    }

    fn keychain() -> PathBuf {
        crate::os::home_dir().join("Library/Keychains/kiln-signing.keychain-db")
    }

    fn password() -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(dir().join("keychain.pass")).context("kiln's signing keychain has no password here")?.trim().to_owned())
    }

    /// `security …`, its output, or why it failed.
    fn security(args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("/usr/bin/security").args(args).stdin(Stdio::null()).output().context("run /usr/bin/security")?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        anyhow::ensure!(out.status.success(), "security {}: {}", args.first().copied().unwrap_or(""), String::from_utf8_lossy(&out.stderr).trim());
        Ok(text)
    }

    fn unlock(pw: &str) -> anyhow::Result<()> {
        security(&["unlock-keychain", "-p", pw, &keychain().to_string_lossy()]).map(|_| ())
    }

    /// The identity's SHA-1, when the keychain has it: `valid` only when macOS trusts it for code
    /// signing.
    fn find(valid: bool) -> Option<String> {
        let kc = keychain();
        let mut args = vec!["find-identity"];
        if valid {
            args.push("-v");
        }
        args.extend(["-p", "codesigning"]);
        let kc = kc.to_string_lossy();
        args.push(&kc);
        let out = security(&args).ok()?;
        out.lines().find(|l| l.contains(NAME)).and_then(|l| l.split_whitespace().nth(1)).map(str::to_owned)
    }

    fn identity() -> Option<String> {
        if !keychain().exists() {
            return None;
        }
        unlock(&password().ok()?).ok()?;
        find(true)
    }

    pub fn state() -> State {
        if !keychain().exists() {
            return State::Missing("kiln has no signing identity on this Mac: run `kiln signing setup` once, so macOS asks for the local network once, not after every upgrade".to_owned());
        }
        match identity() {
            Some(id) => State::Ready(id),
            None => State::Missing("kiln's signing identity here isn't trusted for code signing: run `kiln signing setup` again".to_owned()),
        }
    }

    pub fn sign_as(tmp: &Path, dest: &Path, build: &str) -> anyhow::Result<bool> {
        let Some(id) = identity() else { return Ok(false) };
        let kc = keychain();
        let out = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", &id, "--keychain"])
            .arg(&kc)
            .args(["--identifier", IDENTIFIER, "--timestamp=none"])
            .arg(tmp)
            .stdin(Stdio::null())
            .output()
            .context("run /usr/bin/codesign")?;
        anyhow::ensure!(out.status.success(), "codesign: {}", String::from_utf8_lossy(&out.stderr).trim());
        super::remember(dest, tmp, build)?;
        Ok(true)
    }

    pub fn setup() -> anyhow::Result<Vec<PathBuf>> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let d = dir();
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
        let pass = d.join("keychain.pass");
        if !pass.exists() {
            let bytes: [u8; 24] = rand::random();
            crate::util::write_secret(&pass, hex::encode(bytes).as_bytes(), 0o600)?;
        }
        let pw = password()?;
        let kc = keychain();
        let kcs = kc.to_string_lossy().into_owned();
        if !kc.exists() {
            security(&["create-keychain", "-p", &pw, &kcs])?;
            // It stays unlocked until the Mac restarts (kiln unlocks it again then).
            security(&["set-keychain-settings", &kcs])?;
        }
        unlock(&pw)?;
        // codesign finds the identity through the user's keychain list.
        let listed = security(&["list-keychains", "-d", "user"])?;
        if !listed.contains(&*kcs) {
            let mut list: Vec<String> = listed.lines().map(|l| l.trim().trim_matches('"').to_owned()).filter(|l| !l.is_empty()).collect();
            list.push(kcs.clone());
            let mut args = vec!["list-keychains", "-d", "user", "-s"];
            args.extend(list.iter().map(String::as_str));
            security(&args)?;
        }
        let cert = d.join("cert.pem");
        if find(false).is_none() {
            let cfg = d.join("cert.cnf");
            std::fs::write(&cfg, format!("[req]\ndistinguished_name = dn\nprompt = no\n[dn]\nCN = {NAME}\n[ext]\nbasicConstraints = critical,CA:false\nkeyUsage = critical,digitalSignature\nextendedKeyUsage = critical,codeSigning\n"))?;
            let key = d.join("key.pem");
            let p12 = d.join("identity.p12");
            let ok = |c: &mut Command| -> anyhow::Result<()> {
                let out = c.stdin(Stdio::null()).output()?;
                anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
                Ok(())
            };
            // macOS's own openssl (LibreSSL): its PKCS#12 is what `security import` reads.
            ok(Command::new("/usr/bin/openssl").args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "7300", "-extensions", "ext", "-config"]).arg(&cfg).arg("-keyout").arg(&key).arg("-out").arg(&cert)).context("make the certificate")?;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
            ok(Command::new("/usr/bin/openssl").args(["pkcs12", "-export", "-name", NAME, "-passout"]).arg(format!("pass:{pw}")).arg("-inkey").arg(&key).arg("-in").arg(&cert).arg("-out").arg(&p12)).context("package the identity")?;
            let imported = security(&["import", &p12.to_string_lossy(), "-k", &kcs, "-P", &pw, "-T", "/usr/bin/codesign"]);
            let _ = std::fs::remove_file(&key);
            let _ = std::fs::remove_file(&p12);
            imported.context("put the identity in kiln's keychain")?;
            // codesign may use the key without asking each time.
            security(&["set-key-partition-list", "-S", "apple-tool:,apple:,codesign:", "-s", "-k", &pw, &kcs])?;
        }
        if find(true).is_none() {
            anyhow::ensure!(cert.exists(), "kiln's signing certificate isn't in {}: remove {} and run this again", d.display(), kc.display());
            eprintln!("  macOS asks you to allow a change to your certificate trust settings: allow it, so kiln can sign its own builds.");
            security(&["add-trusted-cert", "-r", "trustRoot", "-p", "codeSign", "-k", &kcs, &cert.to_string_lossy()]).context("have macOS trust kiln's signing certificate for code signing")?;
            anyhow::ensure!(find(true).is_some(), "macOS still doesn't trust kiln's signing certificate for code signing");
        }
        // Every kiln binary installed here: its launchers, the kiln that runs this, and the builds
        // upgrades keep.
        let mut bins: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(crate::config::default_data_dir().join("launcher")) {
            bins.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_none()));
        }
        if let Ok(exe) = std::env::current_exe() {
            bins.push(exe);
        }
        if let Ok(rd) = std::fs::read_dir(crate::config::default_data_dir().join("agent").join("versions")) {
            bins.extend(rd.flatten().map(|e| e.path().join("kiln")).filter(|p| p.is_file()));
        }
        let mut signed = Vec::new();
        for b in bins {
            if super::resign(&b).with_context(|| format!("sign {}", b.display()))? {
                signed.push(b);
            }
        }
        Ok(signed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_signed_here_is_the_build_it_was_while_it_is_the_one_signed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let bin = dir.path().join("kiln");
        std::fs::write(&bin, b"signed bytes")?;
        remember(&bin, &bin, "build123")?;
        let (sha, _) = crate::updates::hash_file(&bin)?;
        assert_eq!(build_of(&bin, &sha).as_deref(), Some("build123"));
        // Replaced since (another build): its own bytes say what it is.
        std::fs::write(&bin, b"other bytes")?;
        let (sha, _) = crate::updates::hash_file(&bin)?;
        assert_eq!(build_of(&bin, &sha), None);
        Ok(())
    }
}
