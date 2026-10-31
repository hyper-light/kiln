//! ctl-12/U03: immutable paths of the existing physical admission owner.
//! Production keeps its flat layout. Private domains exist only through explicit test handles;
//! neither a workspace, a process identity nor an environment override can choose one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Admission(Arc<PathBuf>);

impl Admission {
    pub(crate) fn production() -> Self {
        static DOMAIN: OnceLock<Admission> = OnceLock::new();
        DOMAIN.get_or_init(|| Self(Arc::new(crate::os::home_dir().join(".cache/kiln/admission")))).clone()
    }

    pub(crate) fn root(&self) -> &Path { &self.0 }
    pub(crate) fn ci(&self) -> PathBuf { self.root().join("ci") }

    /// The caller owns and retains this directory through physical retirement and ACK.
    /// Reopening uses this SAME explicit path, never a new implicit/random domain.
    #[cfg(test)]
    pub(crate) fn private(root: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(root.is_absolute(), "private admission authority requires an explicit absolute directory");
        let root = root.canonicalize()?;
        anyhow::ensure!(root.is_dir(), "private admission authority requires an existing owned directory");
        let production = Self::production();
        let declared = std::path::absolute(production.root())?;
        // Reject both resolved destinations and their declaring ancestors. For example,
        // a symlinked ~/.cache must not make HOME look like a disjoint private authority.
        // This validation performs no writes and runs only here, outside owner locks.
        let mut ancestor = declared.as_path();
        let mut suffix = Vec::new();
        loop {
            match ancestor.canonicalize() {
                Ok(mut production) => {
                    for component in suffix.iter().rev() { production.push(component); }
                    anyhow::ensure!(!root.starts_with(&production) && !production.starts_with(&root), "private admission must not overlap the production registry");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
            let Some(parent) = ancestor.parent() else { break };
            suffix.push(ancestor.file_name().ok_or_else(|| anyhow::anyhow!("invalid production admission path"))?.to_owned());
            ancestor = parent;
        }
        Ok(Self(Arc::new(root)))
    }
}
