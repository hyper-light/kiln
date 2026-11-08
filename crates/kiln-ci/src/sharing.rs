//! Who may use a repository's actions and reusable workflows (docs/ci.md W29-R17, R19): GitHub's
//! model, in kiln's terms. The server passes in each repository's setting.

use serde::{Deserialize, Serialize};

/// A repository's Access (W29-R17).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Not shared: its own workflows only, as GitHub's "Not accessible". The default.
    #[default]
    None,
    /// Its owner's repositories, as GitHub's "Accessible from repositories owned by" a user or in
    /// an organization: the owner is the name's first half.
    Owner,
    /// Everyone on this kiln, as GitHub's enterprise level, or a public repository.
    Everyone,
}

impl Access {
    /// `.kiln/config.yaml`'s `share:` (`none`, `owner` or `everyone`), any case.
    pub fn of_share(share: &str) -> Option<Access> {
        match share.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Access::None),
            "owner" => Some(Access::Owner),
            "everyone" => Some(Access::Everyone),
            _ => None,
        }
    }

    /// GitHub's `access_level` (`GET /repos/{owner}/{repo}/actions/permissions/access`): `none`,
    /// `user` and `organization` (the owner's), or `enterprise`.
    pub fn of_github(level: &str) -> Option<Access> {
        match level.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Access::None),
            "user" | "organization" => Some(Access::Owner),
            "enterprise" => Some(Access::Everyone),
            _ => None,
        }
    }

    /// As `share:` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Access::None => "none",
            Access::Owner => "owner",
            Access::Everyone => "everyone",
        }
    }
}

/// A repository's owner: its name's first half, as GitHub compares them (any case).
fn owner_of(repo: &str) -> String {
    repo.split('/').next().unwrap_or_default().to_ascii_lowercase()
}

/// Whether workflows of repository `caller` (`owner/name`) may use what `publisher` shares at
/// `access` (W29-R17, R19): its own workflows always may. A refusal says why, naming the
/// repository, its setting and the caller, and the `share:` that would let it (KCI4022's words).
pub fn allows(access: Access, publisher: &str, caller: &str) -> Result<(), String> {
    if publisher.eq_ignore_ascii_case(caller) {
        return Ok(());
    }
    let same_owner = owner_of(publisher) == owner_of(caller);
    match access {
        Access::Everyone => Ok(()),
        Access::Owner if same_owner => Ok(()),
        _ => {
            let would = if same_owner { Access::Owner } else { Access::Everyone };
            Err(format!("`{publisher}` isn't shared with `{caller}` (its `share:` is `{}`): `share: {}` in `{publisher}`'s `.kiln/config.yaml` would let it", access.as_str(), would.as_str()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r17_access_levels() {
        assert_eq!(Access::default(), Access::None, "not shared until it says so");
        // A repository's own workflows always may.
        for a in [Access::None, Access::Owner, Access::Everyone] {
            assert_eq!(allows(a, "acme/kit", "acme/kit"), Ok(()), "{a:?}");
            assert_eq!(allows(a, "acme/kit", "ACME/Kit"), Ok(()), "{a:?}");
        }
        // Not shared: nobody else.
        assert!(allows(Access::None, "acme/kit", "acme/app").is_err());
        // The owner's: its other repositories, not another owner's.
        assert_eq!(allows(Access::Owner, "acme/kit", "acme/app"), Ok(()));
        assert_eq!(allows(Access::Owner, "acme/kit", "Acme/app"), Ok(()));
        assert!(allows(Access::Owner, "acme/kit", "other/app").is_err());
        // Everyone's.
        assert_eq!(allows(Access::Everyone, "acme/kit", "other/app"), Ok(()));
        // A refusal names the repository, its setting, the caller and what would let it.
        let why = allows(Access::None, "acme/kit", "acme/app").err().unwrap_or_default();
        for word in ["`acme/kit`", "`acme/app`", "`none`", "share: owner", ".kiln/config.yaml"] {
            assert!(why.contains(word), "{word}: {why}");
        }
        assert!(allows(Access::Owner, "acme/kit", "other/app").err().unwrap_or_default().contains("share: everyone"));
        for (s, a) in [("none", Access::None), ("Owner", Access::Owner), (" everyone ", Access::Everyone)] {
            assert_eq!(Access::of_share(s), Some(a), "{s}");
            assert_eq!(Access::of_share(a.as_str()), Some(a));
        }
        assert_eq!(Access::of_share("public"), None);
    }

    #[test]
    fn w29_r17_github_levels() {
        for (level, a) in [("none", Access::None), ("user", Access::Owner), ("organization", Access::Owner), ("enterprise", Access::Everyone)] {
            assert_eq!(Access::of_github(level), Some(a), "{level}");
        }
        assert_eq!(Access::of_github("everyone"), None, "kiln's word isn't GitHub's");
    }
}
