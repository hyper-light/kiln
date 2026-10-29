//! Kubernetes' names of a node's containers (`docs/observe.md` §10.10), from the links the kubelet
//! keeps in `/var/log/containers`: `<pod>_<namespace>_<container>-<id>.log`. Readable without the
//! engine's API, whatever the engine (containerd, CRI-O, Docker's shim): a pod's, a namespace's and
//! a container's names hold no `_` (Kubernetes' DNS-1123 names), so the name splits at its two.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

/// A container's pod, as Kubernetes names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pod {
    pub namespace: String,
    pub pod: String,
    pub container: String,
}

/// A link's name read: its container's id and pod.
pub fn parse(name: &str) -> Option<(String, Pod)> {
    let stem = name.strip_suffix(".log")?;
    let (rest, id) = stem.rsplit_once('-')?;
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut p = rest.splitn(3, '_');
    let (pod, namespace, container) = (p.next()?, p.next()?, p.next()?);
    if pod.is_empty() || namespace.is_empty() || container.is_empty() || container.contains('_') {
        return None;
    }
    Some((id.to_owned(), Pod { namespace: namespace.to_owned(), pod: pod.to_owned(), container: container.to_owned() }))
}

/// Every container's pod the links in `dir` name, by its id; at most `bound`.
pub fn names(dir: &Path, bound: usize) -> HashMap<String, Pod> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str().and_then(parse)).take(bound).collect()
}

/// The names read again at least this often: a link made in the same tick of the directory's
/// clock as the last reading leaves its time as it was.
pub const AGAIN: Duration = Duration::from_secs(60);

/// [`names`] kept between looks: read again when the directory changes (its time), it changed
/// within a second of the last reading (its clock's tick may hide a change), or [`AGAIN`] passes.
#[derive(Debug, Default)]
pub struct Names {
    /// The directory's time when read, when, and whether it had settled (a second old).
    read: Option<(SystemTime, Instant, bool)>,
    by_id: HashMap<String, Pod>,
}

impl Names {
    /// The names now, read again as said.
    pub fn of(&mut self, dir: &Path, bound: usize, now: Instant) -> &HashMap<String, Pod> {
        let changed = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
        let again = match (self.read, changed) {
            (Some((was, at, settled)), Some(m)) => !settled || m != was || now.duration_since(at) >= AGAIN,
            _ => true,
        };
        if again {
            self.by_id = names(dir, bound);
            self.read = changed.map(|m| (m, now, SystemTime::now().duration_since(m).is_ok_and(|d| d >= Duration::from_secs(1))));
        }
        &self.by_id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const ID: &str = "774edafa40abbe1f8b3c7902d0e11c473a395eda0199044df7115090c8166bff";

    /// The kubelet's links read: a pod's dashes kept, a container's name whole; what is not one of
    /// its links left alone.
    #[test]
    fn kubernetes_links_name_containers() {
        let (id, p) = parse(&format!("checkout-api-7d9f8c6b5-x2kq4_shop_api-{ID}.log")).unwrap();
        assert_eq!((id.as_str(), p.namespace.as_str(), p.pod.as_str(), p.container.as_str()), (ID, "shop", "checkout-api-7d9f8c6b5-x2kq4", "api"));
        for bad in [format!("pod_ns_c-{}.log", &ID[..63]), format!("pod_ns-{ID}.log"), format!("pod_ns_c-{ID}.txt"), "kube-proxy.log".to_owned()] {
            assert!(parse(&bad).is_none(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(format!("web-0_default_nginx-{ID}.log")), "").unwrap();
        std::fs::write(dir.path().join("README"), "").unwrap();
        let n = names(dir.path(), 10);
        assert_eq!(n.get(ID).map(|p| p.pod.as_str()), Some("web-0"));
    }

    /// A directory's handle whose times may be set. Windows opens a directory only with backup
    /// semantics (`FILE_FLAG_BACKUP_SEMANTICS`), and sets times through one with
    /// `FILE_WRITE_ATTRIBUTES`.
    #[cfg(windows)]
    fn dir_handle(p: &std::path::Path) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new().access_mode(0x0100).custom_flags(0x0200_0000).open(p)
    }

    #[cfg(not(windows))]
    fn dir_handle(p: &std::path::Path) -> std::io::Result<std::fs::File> {
        std::fs::File::open(p)
    }

    /// Kept between looks: read again when a link comes or goes (the directory's time), at once
    /// while it is a second old (its clock's tick may hide another), and every [`AGAIN`].
    #[test]
    fn names_are_read_again_when_the_links_change() {
        let dir = tempfile::tempdir().unwrap();
        let mut kept = Names::default();
        let now = Instant::now();
        assert!(kept.of(dir.path(), 10, now).is_empty());
        std::fs::write(dir.path().join(format!("web-0_default_nginx-{ID}.log")), "").unwrap();
        assert_eq!(kept.of(dir.path(), 10, now).len(), 1, "a link made: read again");
        // Its time as when read, and settled (seconds past): kept, the directory not read again.
        std::fs::remove_file(dir.path().join(format!("web-0_default_nginx-{ID}.log"))).unwrap();
        dir_handle(dir.path()).unwrap().set_modified(SystemTime::now() - Duration::from_secs(5)).unwrap();
        kept.read = Some((std::fs::metadata(dir.path()).unwrap().modified().unwrap(), now, true));
        assert_eq!(kept.of(dir.path(), 10, now).len(), 1, "its time as when read: kept");
        assert!(kept.of(dir.path(), 10, now + AGAIN).is_empty(), "a minute on: read again");
    }
}
