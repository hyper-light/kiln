//! U08: kiln's credentials and async execution for the shared Git read-mirror engine.
//! Hosted repositories and release repositories retain their authoritative write paths.

use std::process::Command;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use kiln_cd::git::Auth;
use sha2::{Digest as _, Sha256};

use crate::hub::Hub;

pub(crate) fn github_token(hub: &Hub) -> Option<String> {
    hub.cfg.gitops.github_token_file.as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// CI's credentials, scoped to the hosts they cover. CD can supply its explicit repository
/// credentials to the same mirror engine without broadening their scope.
pub(crate) fn auth(hub: &Hub) -> Auth {
    let mut headers = crate::githost::git_headers(hub);
    if let Some(token) = github_token(hub) {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        headers.push(("https://github.com/".into(), format!("Authorization: Basic {basic}")));
    }
    Auth { headers, isolated: true, ..Auth::default() }
}

pub(crate) fn command(hub: &Hub) -> Command {
    let mut c = kiln_cd::git::command(None, None, &auth(hub));
    c.stdin(std::process::Stdio::null());
    c
}

/// U08: one physical read mirror per source and effective credential identity. Unrelated host
/// credentials do not split a source; a different reader credential cannot reuse its mirror.
pub(crate) fn read_path(hub: &Hub, url: &str, auth: &Auth) -> PathBuf {
    let mut headers: Vec<_> = auth.headers.iter().filter(|(scope, _)| scope.is_empty() || url.starts_with(scope)).cloned().collect();
    if let Some(header) = &auth.header { headers.push((url.to_owned(), format!("Authorization: {header}"))); }
    // Scope is already enforced above: equivalent headers from CI and CD identify the same
    // reader even when one was configured as a host template and the other as a repository.
    let mut values: Vec<_> = headers.into_iter().map(|(_, value)| value).collect();
    values.sort();
    values.dedup();
    let mut identity = Sha256::new();
    let part = |hash: &mut Sha256, bytes: &[u8]| { hash.update((bytes.len() as u64).to_le_bytes()); hash.update(bytes); };
    part(&mut identity, b"kiln/read-mirror/v1");
    part(&mut identity, url.as_bytes());
    identity.update((values.len() as u64).to_le_bytes());
    for value in values { part(&mut identity, value.as_bytes()); }
    if let Some(key) = &auth.ssh_key {
        identity.update([1]);
        part(&mut identity, key.to_string_lossy().as_bytes());
        part(&mut identity, &std::fs::read(key).unwrap_or_default());
    } else { identity.update([0]); }
    identity.update([u8::from(auth.insecure), u8::from(auth.isolated)]);
    hub.cfg.data("git-read").join(format!("{}.git", hex::encode(identity.finalize())))
}

/// Old logical names remain small discovery entries (including the actions catalog). Resolving
/// one never requires network access or a database read and works across restarts.
pub(crate) fn logical_path(hub: &Hub, old: &Path) -> PathBuf {
    let name = std::fs::read_to_string(old.join("kiln-mirror")).ok();
    match name.as_deref().map(str::trim).filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())) {
        Some(name) => hub.cfg.data("git-read").join(format!("{name}.git")),
        None => old.to_path_buf(),
    }
}

pub(crate) fn remember(old: &Path, canonical: &Path) -> anyhow::Result<()> {
    let name = canonical.file_stem().and_then(|s| s.to_str()).context("a read mirror has no identity")?;
    std::fs::create_dir_all(old)?;
    let tmp = old.join(format!("kiln-mirror-{}", crate::util::new_id()));
    std::fs::write(&tmp, name)?;
    std::fs::rename(tmp, old.join("kiln-mirror"))?;
    Ok(())
}

/// Import an unclassified legacy source once. A logical name already assigned an identity
/// cannot be used to carry cached private data across a credential change.
pub(crate) fn legacy(hub: &Hub, old: &Path) -> Vec<PathBuf> {
    if logical_path(hub, old) == old { vec![old.to_path_buf()] } else { Vec::new() }
}

pub(crate) async fn open_read(hub: &Hub, old: &Path, url: &str, options: kiln_cd::git::Options, timeout: Duration) -> anyhow::Result<kiln_cd::git::Mirror> {
    let auth = auth(hub);
    let canonical = read_path(hub, url, &auth);
    let mirror = kiln_cd::git::Mirror::open_import_using(&canonical, url, auth, options, &legacy(hub, old), |c| run(c, timeout)).await?;
    remember(old, &canonical)?;
    Ok(mirror)
}

/// Keep CI's command deadline when driving the shared mirror engine. This Command-only
/// adapter does not transfer an OwnedOperation's custody to the physical process owner:
/// cancellation-safe mirror release also needs that owner to retain OperationCustody before
/// submission and prove exact subtree retirement. Root exit/EOF or a kill request is not that
/// proof. Shared-mirror callers migrate to the owned seam with that lifecycle integration.
pub(crate) async fn run(cmd: Command, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let out = crate::proc::output(cmd, timeout).await.context("running git")?;
    if !out.status.success() {
        return Err(kiln_cd::git::GitExit::new(out.status, crate::util::truncate(String::from_utf8_lossy(&out.stderr).trim(), 600).to_owned()).into());
    }
    Ok(out.stdout)
}
