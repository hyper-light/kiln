//! Action policy (docs/ci.md W07-R04): which remote actions and reusable workflows a run may use.
//!
//! A policy has `allow` and `deny` lists and `require_sha_pins`. Each list entry is
//! `name[@ref]`. `name` is matched against `owner/repo[/path]`: `*` stays within a `/` segment,
//! `**` crosses them. An entry of two segments covers the paths inside the repositories it
//! matches too (`gradle/*` allows `gradle/actions/setup-gradle`). Without `@ref` an entry takes
//! every ref. `docker://` images are judged only by entries that start with `docker://`.
//!
//! A use passes a policy when no `deny` entry matches it and, if there is an `allow` list, one of
//! its entries does. With `require_sha_pins`, refs must be full commit SHAs. A run passes when
//! every policy that applies passes (the server's, then its repository's: a repository can only
//! narrow what the server allows). Local actions (`./…`, `$/…`) and kiln's built-ins are the
//! run's own and are always allowed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub require_sha_pins: bool,
}

/// Why a use was refused: the diagnostic code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

/// `*` within a segment, `**` across them, everything else literal (ASCII case ignored, as
/// GitHub's owner and repository names are).
fn glob(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p {
            [] => t.is_empty(),
            [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| t.get(i..).is_some_and(|s| go(rest, s))),
            [b'*', rest @ ..] => {
                let seg = t.iter().position(|&b| b == b'/').unwrap_or(t.len());
                (0..=seg).any(|i| t.get(i..).is_some_and(|s| go(rest, s)))
            }
            [c, rest @ ..] => t.first().is_some_and(|x| x.eq_ignore_ascii_case(c)) && t.get(1..).is_some_and(|s| go(rest, s)),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// An entry's name and ref patterns: `name@ref`, or for an image `docker://name:tag` and
/// `docker://name@digest`.
fn split_entry(e: &str) -> (&str, Option<&str>) {
    if e.starts_with("docker://") {
        if let Some((n, d)) = e.split_once('@') {
            return (n, Some(d));
        }
        return match e.rsplit_once(':').filter(|(n, t)| n.len() > "docker".len() && !t.contains('/') && !t.starts_with("//")) {
            Some((n, t)) => (n, Some(t)),
            None => (e, None),
        };
    }
    match e.rsplit_once('@') {
        Some((n, r)) => (n, Some(r)),
        None => (e, None),
    }
}

/// Whether entry `e` matches `name` at `git_ref`.
fn matches(e: &str, name: &str, git_ref: &str) -> bool {
    let (pat, ref_pat) = split_entry(e);
    if ref_pat.is_some_and(|r| !glob(r, git_ref)) {
        return false;
    }
    if glob(pat, name) {
        return true;
    }
    // `owner/repo-glob` covers the paths inside the repositories it matches.
    !pat.starts_with("docker://") && pat.split('/').count() == 2 && {
        let mut segs = name.splitn(3, '/');
        match (segs.next(), segs.next()) {
            (Some(o), Some(r)) => glob(pat, &format!("{o}/{r}")),
            _ => false,
        }
    }
}

/// A full commit SHA.
fn is_sha(r: &str) -> bool {
    r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Splits a `uses:` into the name policies judge and its ref: `owner/repo[/path]` and the ref,
/// or a `docker://` image and its tag or digest. Local and built-in actions give `None`.
pub fn subject(uses: &str) -> Option<(String, String)> {
    if uses.starts_with("./") || uses.starts_with("$/") || uses.starts_with("kiln/") {
        return None;
    }
    // A kiln-hosted repository's action is judged by its `owner/repo/path`, as GitHub's are.
    let uses = uses.strip_prefix("kiln://").unwrap_or(uses);
    if let Some(image) = uses.strip_prefix("docker://") {
        let (name, r) = match image.split_once('@') {
            Some((n, d)) => (n, d.to_owned()),
            None => match image.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                Some((n, t)) => (n, t.to_owned()),
                None => (image, "latest".to_owned()),
            },
        };
        return Some((format!("docker://{name}"), r));
    }
    let (name, r) = uses.split_once('@')?;
    Some((name.to_owned(), r.to_owned()))
}

/// Checks one use against the policies that apply, in order.
pub fn check(policies: &[&Policy], uses: &str) -> Result<(), Refusal> {
    let Some((name, git_ref)) = subject(uses) else { return Ok(()) };
    let docker = name.starts_with("docker://");
    for (i, p) in policies.iter().enumerate() {
        let scope = if i == 0 && policies.len() > 1 { "the server's" } else if policies.len() > 1 { "the repository's" } else { "this server's" };
        let own = |e: &&String| e.starts_with("docker://") == docker;
        if let Some(rule) = p.deny.iter().filter(own).find(|e| matches(e, &name, &git_ref)) {
            return Err(Refusal { code: "KCI4020", message: format!("`{uses}` is denied by {scope} action policy (deny `{rule}`)") });
        }
        let allow: Vec<&String> = p.allow.iter().filter(own).collect();
        if !allow.is_empty() && !allow.iter().any(|e| matches(e, &name, &git_ref)) {
            return Err(Refusal { code: "KCI4020", message: format!("`{uses}` is not allowed by {scope} action policy (allow: {})", allow.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(", ")) });
        }
        if p.require_sha_pins && !docker && !is_sha(&git_ref) {
            return Err(Refusal { code: "KCI4021", message: format!("`{uses}` is not pinned to a commit: {scope} action policy requires a full commit SHA (`@<40 hex digits>`)") });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(allow: &[&str], deny: &[&str], pins: bool) -> Policy {
        Policy { allow: allow.iter().map(|s| (*s).to_owned()).collect(), deny: deny.iter().map(|s| (*s).to_owned()).collect(), require_sha_pins: pins }
    }

    #[test]
    fn w07_ac05_require_sha_pins_refuses_tags_with_kci4021() {
        let pins = p(&[], &[], true);
        let e = check(&[&pins], "actions/checkout@v4").err().unwrap_or(Refusal { code: "", message: String::new() });
        assert_eq!(e.code, "KCI4021");
        assert!(e.message.starts_with("`actions/checkout@v4` is not pinned to a commit"), "{}", e.message);
        assert!(check(&[&pins], "actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683").is_ok());
        assert!(check(&[&pins], "./.github/actions/local").is_ok(), "local actions are the run's own");
        assert!(check(&[&pins], "docker://alpine:3.20").is_ok(), "images are pinned by digest, not by this rule");
    }

    #[test]
    fn w07_ac05_deny_refuses_with_the_rule_named() {
        let pol = p(&["actions/*", "docker/*@v3", "*/setup-*"], &["actions/github-script"], false);
        let e = check(&[&pol], "actions/github-script@v7").err().map(|r| (r.code, r.message));
        assert_eq!(e, Some(("KCI4020", "`actions/github-script@v7` is denied by this server's action policy (deny `actions/github-script`)".into())));
        assert!(check(&[&pol], "actions/checkout@v4").is_ok());
        assert!(check(&[&pol], "docker/login-action@v3").is_ok());
        assert!(check(&[&pol], "docker/login-action@v2").is_err(), "the ref is part of the entry");
        assert!(check(&[&pol], "pnpm/setup-pnpm@v4").is_ok(), "`*` within a segment");
        let other = check(&[&pol], "evil/thing@v1").err().map(|r| r.message).unwrap_or_default();
        assert!(other.contains("is not allowed") && other.contains("`actions/*`"), "{other}");
    }

    #[test]
    fn w07_r04_globs_paths_and_scopes() {
        // Two segments cover the paths inside; `**` crosses segments; names ignore case.
        let pol = p(&["gradle/*", "github/codeql-action/**@v3"], &[], false);
        assert!(check(&[&pol], "gradle/actions/setup-gradle@v4").is_ok());
        assert!(check(&[&pol], "GitHub/CodeQL-Action/upload-sarif@v3").is_ok());
        assert!(check(&[&pol], "github/codeql-action/upload-sarif@v2").is_err());
        // Reusable workflows are judged like actions.
        let wf = p(&["acme/shared/.github/workflows/*@main"], &[], false);
        assert!(check(&[&wf], "acme/shared/.github/workflows/ci.yml@main").is_ok());
        // docker:// entries judge images; other entries don't.
        let img = p(&["actions/*", "docker://alpine:*"], &["docker://*/evil*"], false);
        assert!(check(&[&img], "docker://alpine:3.20").is_ok());
        assert!(check(&[&img], "docker://ubuntu:24.04").is_err());
        assert!(check(&[&img], "docker://ghcr.io/evilcorp:1").is_err());
        let actions_only = p(&["actions/*"], &[], false);
        assert!(check(&[&actions_only], "docker://ubuntu:24.04").is_ok(), "an allow list of actions says nothing of images");
        // A repository narrows the server; it can't widen it.
        let (server, repo) = (p(&["actions/*", "docker/*"], &[], false), p(&["actions/*"], &[], true));
        assert!(check(&[&server, &repo], "docker/login-action@v3").err().is_some_and(|r| r.message.contains("the repository's")));
        assert!(check(&[&server, &repo], "actions/checkout@v4").err().is_some_and(|r| r.code == "KCI4021"));
        assert!(check(&[&server, &p(&["evil/*"], &[], false)], "evil/x@v1").err().is_some_and(|r| r.message.contains("the server's")));
    }
}
