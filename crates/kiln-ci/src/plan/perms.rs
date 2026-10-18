//! A job's token permissions (docs/ci.md W04-R06), GitHub-exact: a job's `permissions` replace
//! the workflow's; once either names scopes, every scope it doesn't name is `none`; with neither,
//! the repository's default applies. Untrusted runs are capped at `read` (and get no ID token).
//! A called workflow can only reduce what its caller has.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::wf::ir::Permissions;
use crate::wf::keys::{KILN_PERMISSION_SCOPES, PERMISSION_SCOPES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    None,
    Read,
    Write,
}

impl Level {
    fn parse(s: &str) -> Level {
        match s {
            // kiln's scopes say what they allow: `binaries: publish`, `environments: deploy`,
            // `registry: push` (W10-R04).
            "write" | "publish" | "deploy" | "push" => Level::Write,
            "read" => Level::Read,
            _ => Level::None,
        }
    }
}

/// The repository's default for jobs that set no permissions (`ci.default_token_permissions`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Default {
    /// GitHub's "restricted" default: `contents` and `packages` read.
    #[default]
    Read,
    /// GitHub's "permissive" default: every scope write but `id-token`.
    Write,
}

/// Every scope, with its level. `metadata` is always `read`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Resolved(pub BTreeMap<String, Level>);

impl Resolved {
    pub fn level(&self, scope: &str) -> Level {
        self.0.get(scope).copied().unwrap_or(Level::None)
    }

    fn all(level: impl Fn(&str) -> Level) -> Resolved {
        let mut m: BTreeMap<String, Level> = PERMISSION_SCOPES.iter().chain(KILN_PERMISSION_SCOPES).map(|s| ((*s).to_owned(), level(s))).collect();
        m.insert("metadata".into(), Level::Read);
        Resolved(m)
    }

    /// The lower of the two, scope by scope.
    pub fn min(&self, other: &Resolved) -> Resolved {
        let mut m = self.0.clone();
        for (k, v) in &mut m {
            *v = (*v).min(other.level(k));
        }
        Resolved(m)
    }
}

/// What `permissions` (a job's, else its workflow's) gives.
pub fn resolve(job: Option<&Permissions>, workflow: Option<&Permissions>, default: Default, trusted: bool) -> Resolved {
    let r = match job.or(workflow) {
        Some(Permissions::ReadAll) => Resolved::all(|s| if s == "id-token" { Level::None } else { Level::Read }),
        Some(Permissions::WriteAll) => Resolved::all(|_| Level::Write),
        Some(Permissions::Scopes(scopes)) => {
            let mut r = Resolved::all(|_| Level::None);
            for (k, v) in scopes {
                r.0.insert(k.clone(), Level::parse(v));
            }
            r.0.insert("metadata".into(), Level::Read);
            r
        }
        None => match default {
            Default::Read => Resolved::all(|s| if s == "contents" || s == "packages" { Level::Read } else { Level::None }),
            Default::Write => Resolved::all(|s| match s {
                "id-token" => Level::None,
                "models" => Level::Read,
                _ => Level::Write,
            }),
        },
    };
    if trusted { r } else { untrusted(&r) }
}

/// The cap on untrusted runs (W03-R08, W18): nothing above `read`, and no ID token.
pub fn untrusted(r: &Resolved) -> Resolved {
    let mut m = r.0.clone();
    for (k, v) in &mut m {
        if k == "id-token" {
            *v = Level::None;
        } else if *v == Level::Write {
            *v = Level::Read;
        }
    }
    Resolved(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(p: &[(&str, &str)]) -> Permissions {
        Permissions::Scopes(p.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect())
    }

    #[test]
    fn github_rules() {
        // Naming scopes sets every other scope to none.
        let r = resolve(None, Some(&scopes(&[("contents", "write")])), Default::Write, true);
        assert_eq!((r.level("contents"), r.level("issues"), r.level("metadata")), (Level::Write, Level::None, Level::Read));
        // A job's permissions replace the workflow's, they don't merge.
        let r = resolve(Some(&scopes(&[("issues", "write")])), Some(&scopes(&[("contents", "write")])), Default::Write, true);
        assert_eq!((r.level("contents"), r.level("issues")), (Level::None, Level::Write));
        // The defaults.
        let r = resolve(None, None, Default::Read, true);
        assert_eq!((r.level("contents"), r.level("packages"), r.level("pull-requests")), (Level::Read, Level::Read, Level::None));
        let r = resolve(None, None, Default::Write, true);
        assert_eq!((r.level("contents"), r.level("id-token")), (Level::Write, Level::None));
        // read-all, write-all, {}.
        assert_eq!(resolve(Some(&Permissions::ReadAll), None, Default::Read, true).level("issues"), Level::Read);
        assert_eq!(resolve(Some(&Permissions::WriteAll), None, Default::Read, true).level("id-token"), Level::Write);
        assert_eq!(resolve(Some(&scopes(&[])), None, Default::Write, true).level("contents"), Level::None);
        // Untrusted: capped at read, no ID token.
        let r = resolve(Some(&Permissions::WriteAll), None, Default::Read, false);
        assert_eq!((r.level("contents"), r.level("id-token")), (Level::Read, Level::None));
        // A called workflow can only reduce.
        let caller = resolve(Some(&scopes(&[("contents", "read")])), None, Default::Read, true);
        let callee = resolve(Some(&Permissions::WriteAll), None, Default::Read, true);
        assert_eq!(callee.min(&caller).level("contents"), Level::Read);
        assert_eq!(callee.min(&caller).level("issues"), Level::None);
    }
}
