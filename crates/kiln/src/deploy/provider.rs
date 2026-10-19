//! What a CD provider answers (docs/deploy-api.md §8.1), whichever it is: KilnCD
//! ([`super::kilncd`]) or Argo CD ([`super::argocd`]).

/// Where a release stands on its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    /// Synced from the release's commit and healthy.
    Yes,
    /// Not yet.
    No,
    /// Its sync failed: why.
    Failed(String),
    /// Synced from the release's commit, and its health is Degraded: why, as the provider says it
    /// (Kubernetes' progress deadline passed, pods that can't start). Evidence it doesn't run, not
    /// a guess from the time it takes.
    Degraded(String),
    /// The provider can't make anything of it (its package doesn't render, or its spec was
    /// refused): why. What ran before runs on.
    Refused(String),
}

/// Why a release couldn't be handed to its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undelivered {
    /// The provider couldn't be reached, or failed on its side: tried again until the timeout.
    Unreachable(String),
    /// The provider refused the release as it is: another try gets the same answer.
    Refused(String),
}

impl Undelivered {
    /// What an HTTP status says of a release a provider was handed: a conflict, a timeout or a
    /// rate limit passes, any other 4xx is the release's own.
    pub fn of_status(status: u16, why: String) -> Self {
        if (400..500).contains(&status) && !matches!(status, 408 | 409 | 429) { Self::Refused(why) } else { Self::Unreachable(why) }
    }
}

impl From<String> for Undelivered {
    fn from(why: String) -> Self {
        Self::Unreachable(why)
    }
}

impl From<&str> for Undelivered {
    fn from(why: &str) -> Self {
        Self::Unreachable(why.to_owned())
    }
}

impl std::fmt::Display for Undelivered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) | Self::Refused(why) => f.write_str(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_release_s_own_errors_are_refusals() {
        assert!(matches!(Undelivered::of_status(400, "invalid spec".into()), Undelivered::Refused(_)));
        assert!(matches!(Undelivered::of_status(403, "permission denied".into()), Undelivered::Refused(_)));
        for passing in [409, 429, 408, 500, 502, 503] {
            assert!(matches!(Undelivered::of_status(passing, String::new()), Undelivered::Unreachable(_)), "{passing} passes");
        }
    }
}
