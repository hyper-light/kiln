//! `kiln-git-sign`: git's signing program for kiln's identities (docs/ci.md W27-R03). git runs it
//! as it runs gpgsm, for a repository whose `gpg.format` is `x509` and `gpg.x509.program` this
//! (kiln's binary by this name, [`program`]): `kiln-git-sign --status-fd=2 -bsau <key>`, the commit
//! on standard input, the signature (CMS, armored as gpgsm armors it:
//! `kiln_ci::identity::gitsig`) on standard output, and gpgsm's status lines on the status
//! descriptor. It signs where the key is, and no key moves:
//!
//! - **An agent's** ([`AGENT_KEY`], as the runner sets an agent session's git): asked of the
//!   runner process that holds the session's key, over its local socket (`KILN_HARNESS_SOCK`),
//!   which signs only for a process of that session, told by its group of processes or its
//!   ancestry, never by what it says.
//! - **A person's** (any other key: `kiln clone` names their identity): with `kiln login`'s key on
//!   their device (`<config>/kiln/identity.key`, 0600), under the certificate kiln issued for it.
//!
//! Checking a signature is kiln's: asked to verify, it says git can't check it here.
//!
//! It's an agent session's credential helper for kiln's server too (`kiln-git-sign credential
//! get`, as [`git_env`] sets it): the agent's own token, which the runner asks the server for, for
//! that session, told apart as it is for signing; so the agent's push goes as the agent, never as
//! its person.

use std::io::{Read as _, Write as _};

use anyhow::Context as _;
use base64::Engine as _;
use kiln_ci::identity::gitsig;
use sha2::Digest as _;

/// The name git runs it by.
pub const NAME: &str = "kiln-git-sign";

/// The signing key that means the agent session the asking process runs in.
pub const AGENT_KEY: &str = "kiln-agent";

/// The signing key that means this device's person, `kiln login`'s identity (`kiln clone` sets it).
pub const DEVICE_KEY: &str = "kiln-device";

/// Runs as git's signing program: what it was asked, its exit code.
pub fn main(args: &[String]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{NAME}: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    if args.first().is_some_and(|a| a == "credential") {
        return credential(args.get(1).map_or("", String::as_str));
    }
    let fd =args.iter().find_map(|a| a.strip_prefix("--status-fd=").map(str::to_owned)).or_else(|| args.iter().position(|a| a == "--status-fd").and_then(|i| args.get(i + 1).cloned()));
    if args.iter().any(|a| a == "--verify") {
        // 9: no public key, as gpgsm says it; git shows "can't check".
        status(fd.as_deref(), "[GNUPG:] NEWSIG\n[GNUPG:] ERRSIG 0000000000000000 19 8 00 0 9\n")?;
        anyhow::bail!("kiln checks commits' signatures itself, against its root (docs/ci.md W27-R03); git can't check them here");
    }
    let key = signing_key(args).ok_or_else(|| anyhow::anyhow!("say which key signs: -u <key> (git's user.signingkey)"))?;
    let mut payload = Vec::new();
    std::io::stdin().read_to_end(&mut payload).context("read what to sign")?;
    let (armored, fingerprint) = sign(&key, &payload)?;
    std::io::stdout().write_all(armored.as_bytes()).context("write the signature")?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    // git looks for "\n[GNUPG:] SIG_CREATED ": a line comes before it, as gpgsm's does.
    status(fd.as_deref(), &format!("[GNUPG:] BEGIN_SIGNING H8\n[GNUPG:] SIG_CREATED D 19 8 00 {now} {fingerprint}\n"))
}

/// The key `-u <key>` names, gpg's way: alone, at the end of combined flags (`-bsau <key>`), or
/// as `--local-user`.
fn signing_key(args: &[String]) -> Option<String> {
    args.iter().find_map(|a| a.strip_prefix("--local-user=").map(str::to_owned)).or_else(|| {
        let at = args.iter().position(|a| a == "--local-user" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('u')))?;
        args.get(at + 1).cloned()
    })
}

/// Writes gpgsm's status lines where git asked: descriptor 1 (standard output) or 2 (standard
/// error), the two git uses.
fn status(fd: Option<&str>, lines: &str) -> anyhow::Result<()> {
    match fd {
        Some("1") => std::io::stdout().write_all(lines.as_bytes()),
        _ => std::io::stderr().write_all(lines.as_bytes()),
    }
    .context("write the status")
}

/// `payload` signed as `key` says: the armored signature, and the signer's certificate's
/// fingerprint (SHA-256, hex) for the status line.
fn sign(key: &str, payload: &[u8]) -> anyhow::Result<(String, String)> {
    let message = gitsig::to_sign(payload).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (signature, certificate) = if key == AGENT_KEY { by_the_runner(&message)? } else { with_device_key(&message)? };
    let armored = gitsig::armor(payload, &signature, &[&certificate]).map_err(|e| anyhow::anyhow!("{e}"))?;
    let fingerprint = sha2::Sha256::digest(&certificate).iter().map(|b| format!("{b:02X}")).collect();
    Ok((armored, fingerprint))
}

/// An agent's signature: asked of the runner holding its session's key (`harness::socket`). Also
/// its verdicts' and comments' (W27-R04).
pub(crate) fn by_the_runner(message: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let answer = crate::harness::cli::request(&crate::harness::socket::Request::Sign { message: b64.encode(message) }).context("the runner holding this agent's key didn't sign")?;
    let field = |k: &str| -> anyhow::Result<Vec<u8>> {
        let s = answer.get(k).and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("the runner's answer has no {k}"))?;
        b64.decode(s).with_context(|| format!("the runner's {k} isn't base64"))
    };
    Ok((field("signature")?, field("certificate")?))
}

/// A person's signature, with `kiln login`'s key on this device. Also their verdicts' and comments'
/// (W27-R04).
pub(crate) fn with_device_key(message: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    use rcgen::SigningKey as _;
    let dir = crate::identities::device_dir();
    let key = std::fs::read_to_string(dir.join("identity.key")).with_context(|| format!("this device has no identity to sign with ({}): `kiln login` makes one", dir.display()))?;
    let cert = std::fs::read_to_string(dir.join("identity.pem")).with_context(|| format!("this device's identity has no certificate ({}): `kiln login` again", dir.display()))?;
    let signature = rcgen::KeyPair::from_pem(&key).context("this device's key")?.sign(message)?;
    Ok((signature, crate::pki::der_of(&cert)?))
}

/// git's credential helper for an agent session (`credential <op>`, [`git_env`]): for `get`, the
/// agent's own token for kiln ([`agent_credential`]); for `erase` (kiln refused it), the runner
/// asks for another next time. Nothing else answers for kiln's server in the session, so short of
/// the agent's own, git stops (`quit`) rather than push as anyone else.
fn credential(op: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).context("read git's question")?;
    match op {
        "get" => match agent_credential() {
            Some(Ok(token)) => std::io::stdout().write_all(format!("username=kiln\npassword={token}\n").as_bytes()).context("write the credential"),
            failed => {
                std::io::stdout().write_all(b"quit=1\n").context("write the answer")?;
                match failed {
                    Some(Err(e)) => Err(e),
                    _ => anyhow::bail!("only an agent session kiln runs here takes a credential for kiln from its runner"),
                }
            }
        },
        "erase" => {
            let _ = crate::harness::cli::request(&crate::harness::socket::Request::Credential { forget: true });
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The token of the agent session this process runs in, for kiln's server: asked of the runner
/// that runs it (`harness::socket`), which tells the session by this process's group of processes
/// or its ancestry, and asks the server for the agent's own. `None`: the runner says this process
/// is no agent's; an error: no token, so it pushes as no one rather than as anyone else.
fn agent_credential() -> Option<anyhow::Result<String>> {
    match crate::harness::cli::request(&crate::harness::socket::Request::Credential { forget: false }) {
        Ok(a) if a.get("agent").and_then(serde_json::Value::as_bool) == Some(false) => None,
        Ok(a) => Some(a.get("password").and_then(serde_json::Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("the runner's answer has no token"))),
        Err(e) => Some(Err(e.context("this agent's runner gave it no credential for kiln"))),
    }
}

/// git's signing program in `dir`: kiln's own binary by [`NAME`] (a hard link, else a copy), made
/// or made again when the binary changed. Its path, for `gpg.x509.program`.
pub fn program(dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("this binary's path")?;
    let path = dir.join(if cfg!(windows) { format!("{NAME}.exe") } else { NAME.to_owned() });
    let current = matches!((std::fs::metadata(&exe), std::fs::metadata(&path)), (Ok(x), Ok(y)) if x.len() == y.len() && x.modified().ok() <= y.modified().ok());
    if !current {
        std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
        let tmp = dir.join(format!(".{NAME}-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        if std::fs::hard_link(&exe, &tmp).is_err() {
            std::fs::copy(&exe, &tmp).with_context(|| format!("copy {} to {}", exe.display(), tmp.display()))?;
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("place {}", path.display()))?;
    }
    Ok(path)
}

/// [`program`] in this user's kiln state, in a folder of this binary's own (`<state>/kiln/git-sign/
/// <its path's hash>`): two kilns of one user (a runner and a test's, a CLI and a runner) never
/// take each other's.
pub fn installed() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("this binary's path")?;
    let tag: String = sha2::Sha256::digest(exe.to_string_lossy().as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    program(&crate::os::state_home().join("kiln").join("git-sign").join(tag))
}

/// git's configuration that signs every commit with [`NAME`] as `key`, each `(name, value)`.
pub fn git_config(program: &std::path::Path, key: &str) -> Vec<(&'static str, String)> {
    vec![
        ("gpg.format", "x509".to_owned()),
        ("gpg.x509.program", program.to_string_lossy().into_owned()),
        ("user.signingkey", key.to_owned()),
        ("commit.gpgsign", "true".to_owned()),
        ("tag.gpgsign", "true".to_owned()),
    ]
}

/// An agent session's git as its environment (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`,
/// `GIT_CONFIG_VALUE_<n>`), after the `count` entries an inherited environment has already:
/// signing every commit as `key` ([`git_config`]), and taking its credential for the kiln server
/// at each of `kiln`'s URLs from [`NAME`] alone (`credential`, the agent's own: an empty helper
/// first drops every one the person's configuration names there). So its git signs and pushes as
/// the agent, whatever the person's own configuration says.
pub fn git_env(program: &std::path::Path, key: Option<&str>, kiln: &[String], count: usize) -> Vec<(String, String)> {
    let mut config: Vec<(String, String)> = key.map(|k| git_config(program, k)).unwrap_or_default().into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    // git runs a `!` helper with the shell.
    let ours = format!("!'{}' credential", program.to_string_lossy().replace('\'', r"'\''"));
    for url in kiln {
        let helper = format!("credential.{url}.helper");
        config.push((helper.clone(), String::new()));
        config.push((helper, ours.clone()));
    }
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), (count + config.len()).to_string())];
    for (i, (k, v)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{}", count + i), k));
        env.push((format!("GIT_CONFIG_VALUE_{}", count + i), v));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn w27_r03_git_sign_reads_gits_arguments() {
        assert_eq!(signing_key(&args(&["--status-fd=2", "-bsau", "kiln-agent"])).as_deref(), Some("kiln-agent"));
        assert_eq!(signing_key(&args(&["-b", "-s", "-a", "-u", "kiln://hyper-light/person/sam"])).as_deref(), Some("kiln://hyper-light/person/sam"));
        assert_eq!(signing_key(&args(&["--local-user=sam"])).as_deref(), Some("sam"));
        assert_eq!(signing_key(&args(&["--status-fd=1", "--verify", "/tmp/sig", "-"])), None);
    }

    #[test]
    fn w27_r03_an_agents_git_signs_as_the_agent_whatever_came_before() {
        let env = git_env(std::path::Path::new("/s/kiln-git-sign"), Some(AGENT_KEY), &[], 2);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("7"), "after the two the session had");
        assert_eq!((get("GIT_CONFIG_KEY_2"), get("GIT_CONFIG_VALUE_2")), (Some("gpg.format"), Some("x509")));
        assert_eq!((get("GIT_CONFIG_KEY_3"), get("GIT_CONFIG_VALUE_3")), (Some("gpg.x509.program"), Some("/s/kiln-git-sign")));
        assert_eq!((get("GIT_CONFIG_KEY_4"), get("GIT_CONFIG_VALUE_4")), (Some("user.signingkey"), Some(AGENT_KEY)));
        assert_eq!((get("GIT_CONFIG_KEY_5"), get("GIT_CONFIG_VALUE_5")), (Some("commit.gpgsign"), Some("true")));
    }

    #[test]
    fn w27_r03_an_agents_git_takes_its_own_credential_for_kiln_alone() {
        let kiln = ["http://10.0.0.7:7070".to_owned(), "https://kiln.example.com".to_owned()];
        let env = git_env(std::path::Path::new("/s/it's/kiln-git-sign"), Some(AGENT_KEY), &kiln, 0);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("9"));
        // At each of kiln's URLs, the person's helpers dropped first, then the agent's own.
        assert_eq!((get("GIT_CONFIG_KEY_5"), get("GIT_CONFIG_VALUE_5")), (Some("credential.http://10.0.0.7:7070.helper"), Some("")));
        assert_eq!((get("GIT_CONFIG_KEY_6"), get("GIT_CONFIG_VALUE_6")), (Some("credential.http://10.0.0.7:7070.helper"), Some(r"!'/s/it'\''s/kiln-git-sign' credential")));
        assert_eq!((get("GIT_CONFIG_KEY_7"), get("GIT_CONFIG_VALUE_7")), (Some("credential.https://kiln.example.com.helper"), Some("")));
        assert_eq!((get("GIT_CONFIG_KEY_8"), get("GIT_CONFIG_VALUE_8")), (Some("credential.https://kiln.example.com.helper"), Some(r"!'/s/it'\''s/kiln-git-sign' credential")));
        // A server that gives none: signing alone; one that issues no identities: the credential alone.
        assert_eq!(git_env(std::path::Path::new("/s/kiln-git-sign"), Some(AGENT_KEY), &[], 0).len(), 11);
        let only = git_env(std::path::Path::new("/s/kiln-git-sign"), None, &kiln[..1], 0);
        assert_eq!(only.first().map(|(_, v)| v.as_str()), Some("2"), "{only:?}");
    }
}
