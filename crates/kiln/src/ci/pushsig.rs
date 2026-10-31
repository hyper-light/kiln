//! What a push's commits are signed as, checked as a repository kiln hosts takes them (docs/ci.md
//! W27-R03). Once git has a push's objects, quarantined, and before any ref moves, an `update` hook
//! of kiln's own runs for each ref: kiln's binary ([`HOOK_ARG`]), in a folder of the push's own
//! holding what the server knew as the push began (its organization, root and revocations, the
//! repository's `signatures:` and whether it requires them, whom an agent pushing acts for). It
//! reads the commits the ref brings that the repository hasn't got, checks each one's signature
//! as of now (`kiln_ci::identity::gitsig`), judges them (`kiln_ci::identity::push`) and writes its
//! verdict there. Refused, git turns the ref down, and the server puts kiln's words in place of
//! git's "hook declined"; taken, the server names each commit that doesn't verify, where signatures
//! aren't required. Nothing in a push that fails to be checked is taken.

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context as _;
use base64::Engine as _;
use kiln_ci::identity::chain::Revocation;
use kiln_ci::identity::gitsig;
use kiln_ci::identity::push::{Pusher, Seen};
use serde::{Deserialize, Serialize};

use crate::hub::Hub;

/// The argument kiln's binary runs as the hook with: `<kiln> __git-hook update <push's folder>
/// <ref> <old> <new>`.
pub const HOOK_ARG: &str = "__git-hook";

/// What the hook knows of the server, as the push began.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Context {
    org: String,
    /// kiln's root certificate, DER in base64.
    root: String,
    revocations: Vec<Revoked>,
    policy: kiln_ci::land::Signatures,
    /// Whether the repository requires signatures where its policy doesn't say.
    required: bool,
    /// The person an agent that pushes acts for (`admin` for the owner); none for anyone else.
    agent_for: Option<String>,
    /// The SSH keys bound to people still enabled: each person and their key's blob, base64.
    #[serde(default)]
    ssh: Vec<(String, String)>,
}

/// A revocation, as the push's folder keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Revoked {
    serial: Option<String>,
    key: Option<String>,
    at: i64,
    reason: String,
    by: String,
}

/// One ref's verdict, as the hook writes it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Verdict {
    pub refname: String,
    pub refused: Option<String>,
    pub warnings: Vec<String>,
    /// What it found of each commit the ref brought, for the trail (W27-R09).
    #[serde(default)]
    pub commits: Vec<Found>,
}

/// What kiln's hook found of one commit's signature: the signature by its digest (the SHA-256 of
/// it as the commit carries it), and `seen` (`unsigned`, `unverified`, `by`) with its words (why it
/// isn't verified; the identity that signed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Found {
    pub sha: String,
    pub signature: Option<String>,
    pub seen: String,
    pub words: Option<String>,
}

/// A push's folder, readied for kiln's hook (`<data>/pushes/<id>`), gone when this is dropped.
pub struct Prepared {
    dir: PathBuf,
}

impl Prepared {
    /// The `-c` that has git run kiln's hook for this push.
    pub fn config(&self) -> String {
        format!("core.hooksPath={}", self.dir.join("hooks").display())
    }

    /// The hook's verdicts: each ref's it judged.
    pub fn verdicts(&self) -> Vec<Verdict> {
        let Ok(entries) = std::fs::read_dir(self.dir.join("verdicts")) else { return Vec::new() };
        let mut out: Vec<Verdict> = entries.filter_map(Result::ok).filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice(&b).ok()).collect();
        out.sort_by(|a, b| a.refname.cmp(&b.refname));
        out
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Readies a push to repository `repo_id`, whose trunk's `.kiln/review.yaml` says `policy`, for
/// kiln's hook; `agent_for`: whom the agent pushing acts for, when an agent pushes.
pub async fn prepare(hub: &Hub, repo_id: &str, policy: &kiln_ci::land::Signatures, agent_for: Option<String>) -> anyhow::Result<Prepared> {
    let required = hub.store.repo_requires_signatures(repo_id.to_owned()).await?;
    let revocations = crate::identities::revocations(&hub.store).await?.into_iter().map(|r| Revoked { serial: r.serial, key: r.key, at: r.at, reason: r.reason, by: r.by }).collect();
    let b64 = base64::engine::general_purpose::STANDARD;
    let ssh = crate::identities::bound_ssh_keys(&hub.store).await?.into_iter().map(|(person, blob)| (person, b64.encode(blob))).collect();
    let context = Context { org: hub.pki.organization(), root: b64.encode(&hub.pki.ca_der), revocations, policy: policy.clone(), required, agent_for, ssh };
    let pushes = hub.cfg.server.data_dir.join("pushes");
    let dir = pushes.join(crate::util::new_id());
    // The kiln at this binary's path, should an upgrade have put a new one there.
    let exe = crate::os::this_kiln().context("this binary's path, for the hook")?;
    crate::rt::blocking(move || -> anyhow::Result<Prepared> {
        // What a push left behind (its server gone mid-push) goes after a day.
        if let Ok(old) = std::fs::read_dir(&pushes) {
            let day = std::time::Duration::from_secs(86_400);
            for e in old.filter_map(Result::ok) {
                if e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > day) {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let hooks = dir.join("hooks");
        std::fs::create_dir_all(&hooks).with_context(|| format!("make {}", hooks.display()))?;
        std::fs::create_dir_all(dir.join("verdicts"))?;
        let prepared = Prepared { dir };
        std::fs::write(prepared.dir.join("context.json"), serde_json::to_vec(&context)?)?;
        // Built as its library's tests, this binary is the test harness, not kiln: git can't run
        // it as the hook, so those tests' pushes go unchecked (tests/identities.rs checks them
        // with kiln's own binary).
        if cfg!(test) {
            return Ok(prepared);
        }
        let script = format!("#!/bin/sh\nexec {} {HOOK_ARG} update {} \"$@\"\n", quoted(&exe), quoted(&prepared.dir));
        let update = hooks.join("update");
        std::fs::write(&update, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&update, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(prepared)
    })
    .await?
}

/// `path` as one word of a POSIX shell.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Runs as git's `update` hook: refuses the ref (a failure) when its commits aren't signed as the
/// repository requires, or when they can't be checked.
pub fn hook_main(args: &[String]) -> std::process::ExitCode {
    match hook(args) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(e) => {
            eprintln!("kiln: this ref's commits' signatures couldn't be checked, so it isn't taken: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn hook(args: &[String]) -> anyhow::Result<bool> {
    let [kind, dir, refname, _old, new] = args else { anyhow::bail!("say `update <push's folder> <ref> <old> <new>`") };
    anyhow::ensure!(kind == "update", "kiln's hook runs as git's update hook, not {kind}");
    // A deletion brings no commits (whether it may delete is the server's to say, before git).
    if new.bytes().all(|b| b == b'0') {
        return Ok(true);
    }
    let dir = Path::new(dir);
    let context: Context = serde_json::from_slice(&std::fs::read(dir.join("context.json")).context("the push's context")?).context("the push's context")?;
    let root = base64::engine::general_purpose::STANDARD.decode(&context.root).context("kiln's root")?;
    let revocations: Vec<Revocation> = context.revocations.iter().map(|r| Revocation { serial: r.serial.clone(), key: r.key.clone(), at: r.at, reason: r.reason.clone(), by: r.by.clone() }).collect();
    let ssh: Vec<(String, Vec<u8>)> = context.ssh.iter().filter_map(|(person, key)| base64::engine::general_purpose::STANDARD.decode(key).ok().map(|blob| (person.clone(), blob))).collect();
    let against = Against { org: &context.org, root: &root, revocations: &revocations, ssh: &ssh, now: crate::util::now_unix() };
    let mut seen: Vec<(String, Seen)> = Vec::new();
    let mut commits: Vec<Found> = Vec::new();
    for (sha, raw) in new_commits(new)? {
        let s = checked(&raw, &against);
        let (word, words) = match &s {
            Seen::Unsigned => ("unsigned", None),
            Seen::Unverified(why) => ("unverified", Some(why.clone())),
            Seen::By(identity) => ("by", Some(identity.uri().unwrap_or_else(|_| identity.to_string()))),
        };
        commits.push(Found { sha: sha.clone(), signature: gitsig::signature_of(&raw).map(|g| crate::util::sha256_hex(g.as_bytes())), seen: word.to_owned(), words });
        seen.push((sha, s));
    }
    let pusher = context.agent_for.clone().map_or(Pusher::Other, |person| Pusher::Agent { person });
    let judged = kiln_ci::identity::push::judge(&context.policy, context.required, &context.org, refname, &pusher, &seen);
    let taken = judged.refused.is_none();
    let verdict = Verdict { refname: refname.clone(), refused: judged.refused, warnings: judged.warnings, commits };
    let file = dir.join("verdicts").join(format!("{}.json", crate::util::sha256_hex(refname.as_bytes())));
    std::fs::write(&file, serde_json::to_vec(&verdict)?).with_context(|| format!("write {}", file.display()))?;
    Ok(taken)
}

/// What a commit's signature is checked against: kiln's root and revocations, the SSH keys people
/// bound, at the time of the push.
struct Against<'a> {
    org: &'a str,
    root: &'a [u8],
    revocations: &'a [Revocation],
    ssh: &'a [(String, Vec<u8>)],
    now: i64,
}

/// What a commit's signature is: kiln's x509 one, checked against its root and revocations; an
/// SSH one, against the keys people bound (W27-R03); none, or another kind, unverified.
fn checked(raw: &[u8], against: &Against<'_>) -> Seen {
    let Some(signature) = gitsig::signature_of(raw) else { return Seen::Unsigned };
    if signature.contains("BEGIN SSH SIGNATURE") {
        return match kiln_ci::identity::sshsig::verify(&gitsig::payload(raw), &signature, against.ssh) {
            Ok(person) => Seen::By(kiln_ci::identity::chain::Identity::new(against.org, kiln_ci::identity::chain::Actor::Person { name: person.to_owned() })),
            Err(e) => Seen::Unverified(e.to_string()),
        };
    }
    if signature.contains("BEGIN PGP SIGNATURE") {
        return Seen::Unverified("an OpenPGP signature, which kiln doesn't check".to_owned());
    }
    match gitsig::verify(&gitsig::payload(raw), &signature, against.root, against.revocations, against.now) {
        Ok(v) => Seen::By(v.identity),
        Err(e) => Seen::Unverified(e.to_string()),
    }
}

/// The commits `new` brings that the repository has under none of its refs, each with its raw
/// object, read in git's quarantine (the hook's environment points git there).
fn new_commits(new: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let out = Command::new("git").args(["rev-list", new, "--not", "--all"]).output().context("git rev-list")?;
    anyhow::ensure!(out.status.success(), "git rev-list: {}", String::from_utf8_lossy(&out.stderr).trim());
    let shas: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git").args(["cat-file", "--batch"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("git cat-file")?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("git cat-file's input"))?;
    let asked = shas.join("\n") + "\n";
    // Asked on a thread of its own: git answers as it's asked, and a long answer would otherwise
    // wait on a question not yet written.
    let writer = std::thread::spawn(move || stdin.write_all(asked.as_bytes()));
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("git cat-file's output"))?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut commits = Vec::with_capacity(shas.len());
    for sha in &shas {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let mut parts = header.split_whitespace();
        let (Some(_), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) else { anyhow::bail!("git cat-file said {header:?} of {sha}") };
        let size: usize = size.parse().with_context(|| format!("git cat-file's size of {sha}"))?;
        let mut body = vec![0u8; size];
        reader.read_exact(&mut body)?;
        let mut newline = [0u8; 1];
        reader.read_exact(&mut newline)?;
        if kind == "commit" {
            commits.push((sha.clone(), body));
        }
    }
    let _ = writer.join();
    let _ = child.wait();
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w27_r03_the_hook_names_its_folder_and_kiln_however_their_paths_are_spelled() -> anyhow::Result<()> {
        assert_eq!(quoted(Path::new("/srv/kiln data/it's")), r"'/srv/kiln data/it'\''s'");
        let scratch = tempfile::tempdir()?;
        let dir = scratch.path().join("p1");
        std::fs::create_dir_all(dir.join("verdicts"))?;
        let p = Prepared { dir: dir.clone() };
        assert_eq!(p.config(), format!("core.hooksPath={}", dir.join("hooks").display()));
        assert!(p.verdicts().is_empty(), "nothing judged yet");
        let v = Verdict { refname: "refs/heads/main".into(), refused: Some("commit 3f2a1c9 isn't signed".into()), warnings: Vec::new(), commits: Vec::new() };
        std::fs::write(dir.join("verdicts").join("a.json"), serde_json::to_vec(&v)?)?;
        assert_eq!(p.verdicts().first().and_then(|v| v.refused.as_deref()), Some("commit 3f2a1c9 isn't signed"));
        drop(p);
        assert!(!dir.exists(), "its folder goes with it");
        Ok(())
    }
}
