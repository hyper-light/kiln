//! AC-T7.4.1/LB03: the last successfully applied remote offer, in the existing atomic file records.
//! One wire-bounded value per configured peer restores real admission facts before Driver::boot;
//! dense-zero placeholders remain for peers whose facts were never applied or were set aside.
//! A cache is not authority: what open can't restore is set aside and told, never a refused start.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use hex_api::Refusal;
use hex_core::wire::MAX_MESSAGE;
use hex_sim::model::{Cluster, NodeId, NodeSpec, Resources, model_name};
use hex_sim::peer::{Records, Stop};
use serde::Serialize;

use super::{dto, offer};
use crate::records::Files;

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid {
        what: "peer offer cache",
        why: why.into(),
    }
}

fn storage(why: Stop) -> Refusal {
    match why {
        Stop::Full { what, bound } => Refusal::Exhausted { what, bound },
        Stop::Damaged(why) => invalid(why),
        Stop::Io(why) => Refusal::Broken(format!("peer offer cache: {why}")),
        Stop::Crashed => Refusal::Broken("peer offer cache write stopped".into()),
    }
}

fn io(why: std::io::Error) -> Refusal {
    Refusal::Broken(format!("peer offer cache: {why}"))
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_MESSAGE)
        {
            return Err(std::io::Error::other(
                "peer offer cache value exceeds wire bound",
            ));
        }
        self.0
            .try_reserve_exact(bytes.len())
            .map_err(std::io::Error::other)?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded(value: &impl Serialize) -> Result<String, Refusal> {
    let mut bytes = Bounded(Vec::new());
    serde_json::to_writer(&mut bytes, value).map_err(|why| {
        if why.is_io() {
            Refusal::Exhausted {
                what: "peer offer cache bytes",
                bound: MAX_MESSAGE,
            }
        } else {
            invalid(why.to_string())
        }
    })?;
    String::from_utf8(bytes.0).map_err(|why| invalid(why.to_string()))
}

fn name(peer: NodeId) -> String {
    format!("peer-{}", peer.0)
}

/// The peer a canonical record name (`peer-<u32>`) names.
fn peer_named(stem: &str) -> Option<NodeId> {
    stem.strip_prefix("peer-")
        .and_then(|n| n.parse::<u32>().ok())
        .map(NodeId)
        .filter(|peer| name(*peer) == stem)
}

/// A cache entry set aside at open: removed, never followed or applied. A cache is not authority,
/// so a configured peer it named starts uncached until it advertises again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAside {
    /// Its name in the cache folder.
    pub entry: String,
    pub why: Aside,
}

/// Why an entry was set aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aside {
    /// A configured peer's record that today's rules refuse: an older build's offer, or damage.
    Refused { peer: NodeId, why: Refusal },
    /// A record for a peer no longer configured.
    Unconfigured { peer: NodeId },
    /// A link or other file that isn't regular, never followed or read.
    NotAFile,
    /// A regular file named as no record or temporary of this cache, never read.
    Unknown,
}

impl SetAside {
    /// The peer it names, to report against.
    pub fn peer(&self) -> Option<NodeId> {
        match &self.why {
            Aside::Refused { peer, .. } | Aside::Unconfigured { peer } => Some(*peer),
            Aside::NotAFile | Aside::Unknown => None,
        }
    }
}

impl std::fmt::Display for SetAside {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entry = &self.entry;
        match &self.why {
            Aside::Refused { why, .. } => write!(
                f,
                "peer offer cache set {entry} aside, its peer uncached until it advertises again: {why}"
            ),
            Aside::Unconfigured { peer } => write!(
                f,
                "peer offer cache set {entry} aside: peer {} is not configured",
                peer.0
            ),
            Aside::NotAFile => write!(
                f,
                "peer offer cache set {entry} aside unread: not a regular file"
            ),
            Aside::Unknown => write!(
                f,
                "peer offer cache set {entry} aside unread: no record has that name"
            ),
        }
    }
}

fn removed(path: &Path) -> Result<(), Refusal> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(why) => Err(io(why)),
    }
}

/// One pass over the folder keeps each configured peer's regular record, read within the wire
/// bound, and removes everything else, telling `aside` why; crash temporaries go silently, as
/// Files removes them. The folder's size at open bounds the pass, and what it keeps is at most one
/// record per configured peer, so the next open finds no more. A folder inside is refused: this
/// cache never makes one, and removing a tree is not its to do.
fn kept_records(
    path: &Path,
    configured: &BTreeSet<u32>,
    aside: &mut impl FnMut(SetAside),
) -> Result<Vec<(NodeId, String)>, Refusal> {
    let mut kept = Vec::new();
    kept.try_reserve_exact(configured.len())
        .map_err(|_| Refusal::Exhausted {
            what: "peer offer cache records",
            bound: configured.len(),
        })?;
    let listing = match fs::read_dir(path) {
        Ok(listing) => listing,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(kept),
        Err(why) => return Err(io(why)),
    };
    for entry in listing {
        let entry = entry.map_err(io)?;
        // The entry's own type: a link is a link, never its target.
        let kind = entry.file_type().map_err(io)?;
        let file_name = entry.file_name();
        let shown = file_name.to_string_lossy().into_owned();
        if kind.is_dir() {
            return Err(invalid(format!(
                "{shown} is a folder, which this cache never makes; it is its owner's to remove"
            )));
        }
        let (stem, ext) = file_name
            .to_str()
            .and_then(|n| n.rsplit_once('.'))
            .unwrap_or_default();
        let why = match (peer_named(stem), ext) {
            (_, "tmp") if kind.is_file() => {
                removed(&entry.path())?;
                continue;
            }
            (Some(peer), "rec") if !configured.contains(&peer.0) => Aside::Unconfigured { peer },
            (Some(peer), "rec") if kind.is_file() => {
                let file = fs::File::open(entry.path()).map_err(io)?;
                match crate::records::read_record(file, &entry.path()) {
                    Ok(value) if kept.len() < configured.len() => {
                        kept.push((peer, value));
                        continue;
                    }
                    Ok(_) => {
                        return Err(Refusal::Exhausted {
                            what: "peer offer cache records",
                            bound: configured.len(),
                        });
                    }
                    // Past the wire bound or not UTF-8: no offer this cache wrote.
                    Err(Stop::Damaged(why)) => Aside::Refused {
                        peer,
                        why: invalid(why),
                    },
                    Err(why) => return Err(storage(why)),
                }
            }
            _ if kind.is_file() => Aside::Unknown,
            _ => Aside::NotAFile,
        };
        removed(&entry.path())?;
        aside(SetAside { entry: shown, why });
    }
    Ok(kept)
}

fn checked_folder(path: &Path) -> Result<(), Refusal> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_dir() => Err(invalid(
            "cache path must be a directory, not a link or other file",
        )),
        Ok(_) => Ok(()),
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(why) => Err(io(why)),
    }
}

fn checked_models(models: &[String]) -> Result<(), Refusal> {
    encoded(&models)?;
    let mut seen = BTreeSet::new();
    for model in models {
        if model.is_empty() || model == "-" || model_name(model) != *model || !seen.insert(model) {
            return Err(invalid("shared model names must be canonical and unique"));
        }
    }
    Ok(())
}

/// Bounded by the configured peer set and Files' existing record budget. Models retain the same
/// append-only intern-table semantics as offer::decode, with its common-wire byte bound.
#[derive(Debug)]
pub struct Cache {
    files: Files,
    configured: BTreeSet<u32>,
    models: Vec<String>,
}

impl Cache {
    /// Restores each configured peer's cached offer whole or not at all, and commits them to the
    /// caller together only if the open succeeds. What can't be restored is set aside and told to
    /// `aside`; a link is never followed. A refused open, which only a configuration error, a
    /// folder that isn't one or holds one, or a storage error makes, changes no caller node or model.
    pub fn open(
        path: &Path,
        configured: &BTreeSet<u32>,
        cluster: &mut Cluster,
        mut aside: impl FnMut(SetAside),
    ) -> Result<Self, Refusal> {
        checked_models(&cluster.models)?;
        for peer in configured {
            if usize::try_from(*peer)
                .ok()
                .and_then(|at| cluster.nodes.get(at))
                .is_none_or(|node| node.id != NodeId(*peer))
            {
                return Err(invalid("configured peer must name its dense cluster node"));
            }
        }
        checked_folder(path)?;
        let mut kept = kept_records(path, configured, &mut aside)?;
        // Peer order makes the shared model table the same whatever order the folder lists.
        kept.sort_unstable_by_key(|(peer, _)| *peer);
        let mut staged = Cluster::single_owner(cluster.nodes.clone(), Vec::new());
        staged.models = cluster.models.clone();
        for (peer, value) in kept {
            // decode changes the staged models only when it returns the node.
            let restored = serde_json::from_str::<dto::Offer>(&value)
                .map_err(|why| invalid(why.to_string()))
                .and_then(|offer| offer::decode(&offer, peer, &mut staged));
            match restored {
                Ok(spec) => {
                    let slot = usize::try_from(peer.0)
                        .ok()
                        .and_then(|at| staged.nodes.get_mut(at))
                        .filter(|node| node.id == peer)
                        .ok_or_else(|| invalid("cached peer has no matching cluster slot"))?;
                    *slot = spec;
                }
                Err(why) => {
                    let entry = format!("{}.rec", name(peer));
                    removed(&path.join(&entry))?;
                    aside(SetAside {
                        entry,
                        why: Aside::Refused { peer, why },
                    });
                }
            }
        }
        // Files makes a missing folder and flushes the removals; what remains is in its bound.
        let files = Files::open_with_limit(path, configured.len()).map_err(storage)?;
        cluster.nodes = staged.nodes;
        cluster.models = staged.models;
        Ok(Self {
            files,
            configured: configured.clone(),
            models: cluster.models.clone(),
        })
    }

    /// The shell calls this after successful Driver::refresh_node and before any retry/placement.
    /// It treats failure as fatal until restart; no in-memory cache metadata commits before rename.
    pub fn put(&mut self, peer: NodeId, offer: &dto::Offer) -> Result<(), Refusal> {
        if !self.configured.contains(&peer.0) {
            return Err(invalid("cannot cache an unconfigured peer"));
        }
        let text = encoded(offer)?;
        // Decode requires configured membership, not dense slots. One placeholder validates this
        // offer while only the bounded intern table is copied, rather than the whole live fleet.
        let mut staged =
            Cluster::single_owner(vec![NodeSpec::new(peer, Resources::default())], Vec::new());
        staged.models = self.models.clone();
        offer::decode(offer, peer, &mut staged)?;
        self.files.put(&name(peer), &text).map_err(storage)?;
        self.models = staged.models;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_sim::model::{Class, Classes, Device, ModelId};

    fn configured() -> BTreeSet<u32> {
        BTreeSet::from([1])
    }
    fn cluster() -> Cluster {
        Cluster::single_owner(
            vec![
                NodeSpec::new(NodeId(0), Resources::default()),
                NodeSpec::new(NodeId(1), Resources::default()),
            ],
            Vec::new(),
        )
    }
    // A peer offering a VM class must carry its immutable runtime cost (offer::checked_shape),
    // so these offers come from OCI-only peers, as offer.rs's own fixtures do.
    fn oci_peer(id: NodeId, capacity: Resources) -> NodeSpec {
        let mut spec = NodeSpec::new(id, capacity);
        spec.classes = Classes::NONE.with(Class::Oci);
        spec
    }
    fn advertised() -> dto::Offer {
        let mut spec = oci_peer(
            NodeId(1),
            Resources {
                cpu_milli: 4_000,
                mem_bytes: 8 << 30,
                gpus: 2,
                ..Resources::default()
            },
        );
        spec.devices = vec![
            Device::gpu(ModelId(0), 8 << 30),
            Device::gpu(ModelId(1), 16 << 30),
        ];
        offer::from_local(&spec, &["nvidia-rtx".into(), "amd-radeon".into()]).unwrap()
    }

    fn no_aside(aside: SetAside) {
        panic!("this open set {aside} aside");
    }

    // Opens, collecting what it set aside in name order: a folder lists in no fixed order.
    fn opened(
        dir: &Path,
        peers: &BTreeSet<u32>,
        facts: &mut Cluster,
    ) -> Result<Vec<SetAside>, Refusal> {
        let mut aside = Vec::new();
        Cache::open(dir, peers, facts, |note| aside.push(note))?;
        aside.sort_by(|a, b| a.entry.cmp(&b.entry));
        Ok(aside)
    }

    /// AC-T7.4.1/LB03: exactly one durable offer and temporary per configured peer fit, and crash
    /// temporaries go. Do leave a record behind for a peer no longer configured; expect it set aside
    /// (removed, told as unconfigured), every configured offer restored, the folder at its bound.
    #[test]
    fn cache_read_count_is_configured_peer_cardinality() {
        let dir = tempfile::tempdir().unwrap();
        let mut facts = cluster();
        facts
            .nodes
            .push(NodeSpec::new(NodeId(2), Resources::default()));
        let peers = BTreeSet::from([1, 2]);
        let mut cache = Cache::open(dir.path(), &peers, &mut facts, no_aside).unwrap();
        cache.put(NodeId(1), &advertised()).unwrap();
        let second = offer::from_local(&oci_peer(NodeId(2), Resources::default()), &[]).unwrap();
        cache.put(NodeId(2), &second).unwrap();
        drop(cache);
        let before = fs::read(dir.path().join("peer-1.rec")).unwrap();
        for peer in &peers {
            fs::write(dir.path().join(format!("peer-{peer}.tmp")), "torn").unwrap();
        }
        Cache::open(dir.path(), &peers, &mut facts, no_aside).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), peers.len());
        fs::write(dir.path().join("peer-3.rec"), "unexpected").unwrap();
        let original = facts.clone();
        assert_eq!(
            opened(dir.path(), &peers, &mut facts).unwrap(),
            [SetAside {
                entry: "peer-3.rec".into(),
                why: Aside::Unconfigured { peer: NodeId(3) },
            }]
        );
        assert_eq!(facts, original);
        assert_eq!(fs::read(dir.path().join("peer-1.rec")).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), peers.len());
        let empty = tempfile::tempdir().unwrap();
        let mut zero =
            Cache::open(empty.path(), &BTreeSet::new(), &mut cluster(), no_aside).unwrap();
        assert!(zero.put(NodeId(1), &advertised()).is_err());
        assert!(fs::read_dir(empty.path()).unwrap().next().is_none());
    }

    /// AC-T7.4.1/LB03: the latest applied offer survives reboot and replaces zero placeholders with
    /// full facts, remapping named GPUs into the rebooted process's independently ordered table.
    #[test]
    fn cached_offer_survives_reboot_and_remaps_models() {
        let dir = tempfile::tempdir().unwrap();
        let mut before = cluster();
        let mut cache = Cache::open(dir.path(), &configured(), &mut before, no_aside).unwrap();
        let mut offer = advertised();
        cache.put(NodeId(1), &offer).unwrap();
        let mut spec: NodeSpec = serde_json::from_str(&offer.spec).unwrap();
        spec.capacity.mem_bytes += 1;
        offer = offer::from_local(&spec, &offer.models).unwrap();
        cache.put(NodeId(1), &offer).unwrap();
        drop(cache);
        let mut after = cluster();
        after.models = vec!["amd-radeon".into(), "nvidia-rtx".into()];
        Cache::open(dir.path(), &configured(), &mut after, no_aside).unwrap();
        assert_eq!(after.nodes[1].capacity, spec.capacity);
        assert_eq!(after.nodes[1].devices[0].model, ModelId(1));
        assert_eq!(after.nodes[1].devices[1].model, ModelId(0));
        assert_eq!(after.nodes[1].devices[0].size, spec.devices[0].size);
        assert_eq!(after.models.len(), 2);
        assert_eq!(Files::open(dir.path()).unwrap().all().unwrap().len(), 1);
    }

    /// AC-T7.4.1/LB03, a cache is not authority: do open over an unconfigured peer's record, an
    /// alias, a stray file, malformed JSON, and an offer naming a model it doesn't carry; expect
    /// each set aside (removed, told by type), the open to succeed, and no caller fact changed.
    #[test]
    fn unknown_peers_and_corrupt_cached_records_are_set_aside() {
        let good = serde_json::to_string(&advertised()).unwrap();
        let mut bad = advertised();
        bad.models.clear();
        let unknown_model = Refusal::Invalid {
            what: "peer offer",
            why: "device references an unknown offered model".into(),
        };
        for (entry, value, why) in [
            (
                "peer-2.rec",
                good.clone(),
                Some(Aside::Unconfigured { peer: NodeId(2) }),
            ),
            ("peer-01.rec", good, Some(Aside::Unknown)),
            (".DS_Store", "finder".into(), Some(Aside::Unknown)),
            ("peer-1.rec", "{".into(), None),
            (
                "peer-1.rec",
                serde_json::to_string(&bad).unwrap(),
                Some(Aside::Refused {
                    peer: NodeId(1),
                    why: unknown_model,
                }),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(entry), &value).unwrap();
            let mut facts = cluster();
            let before = facts.clone();
            let aside = opened(dir.path(), &configured(), &mut facts).unwrap();
            assert_eq!(aside.len(), 1, "{entry}");
            assert_eq!(aside[0].entry, entry);
            match why {
                Some(why) => assert_eq!(aside[0].why, why, "{entry}"),
                // Malformed JSON: refused by its parser, under this cache's name.
                None => assert!(
                    matches!(
                        &aside[0].why,
                        Aside::Refused {
                            peer: NodeId(1),
                            why: Refusal::Invalid {
                                what: "peer offer cache",
                                ..
                            },
                        }
                    ),
                    "{:?}",
                    aside[0]
                ),
            }
            assert_eq!(facts, before, "{entry}");
            assert!(
                fs::read_dir(dir.path()).unwrap().next().is_none(),
                "{entry}"
            );
        }
    }

    /// AC-T7.4.1/LB03: do open over peer 1's valid offer and peer 2's corrupt one; expect peer 2's
    /// set aside whole, nothing of it applied, and peer 1's restored whole with its own models.
    #[test]
    fn boot_cache_loading_is_atomic_across_peers() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Files::open(dir.path()).unwrap();
        files
            .put("peer-1", &serde_json::to_string(&advertised()).unwrap())
            .unwrap();
        files.put("peer-2", "broken").unwrap();
        let mut facts = cluster();
        facts
            .nodes
            .push(NodeSpec::new(NodeId(2), Resources::default()));
        let placeholder = facts.nodes[2].clone();
        let aside = opened(dir.path(), &BTreeSet::from([1, 2]), &mut facts).unwrap();
        assert_eq!(aside.len(), 1);
        assert_eq!(aside[0].entry, "peer-2.rec");
        assert!(matches!(
            aside[0].why,
            Aside::Refused {
                peer: NodeId(2),
                ..
            }
        ));
        let mut restored: NodeSpec = serde_json::from_str(&advertised().spec).unwrap();
        restored.devices[0].model = ModelId(0);
        restored.devices[1].model = ModelId(1);
        assert_eq!(facts.nodes[1], restored);
        assert_eq!(facts.nodes[2], placeholder);
        assert_eq!(facts.models, ["nvidia-rtx", "amd-radeon"]);
        assert!(dir.path().join("peer-1.rec").is_file());
        assert!(!dir.path().join("peer-2.rec").exists());
    }

    /// AC-T7.4.1/LB03, CLAUDE.md §2: do open over a record one byte past the wire bound; expect it
    /// set aside unread past that bound and no caller fact changed.
    #[test]
    fn an_oversized_record_is_set_aside_past_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("peer-1.rec"), "x".repeat(MAX_MESSAGE + 1)).unwrap();
        let mut facts = cluster();
        let before = facts.clone();
        let aside = opened(dir.path(), &configured(), &mut facts).unwrap();
        assert!(
            matches!(
                &aside[..],
                [SetAside {
                    why: Aside::Refused {
                        peer: NodeId(1),
                        why: Refusal::Invalid { why, .. },
                    },
                    ..
                }] if why.contains("byte bound")
            ),
            "{aside:?}"
        );
        assert_eq!(facts, before);
        assert!(!dir.path().join("peer-1.rec").exists());
    }

    /// CLAUDE.md §2/AC-T7.4.1: an oversized replacement is refused before it alters durable state.
    #[test]
    fn oversized_offer_cannot_replace_a_valid_cached_offer() {
        let dir = tempfile::tempdir().unwrap();
        let mut facts = cluster();
        let mut cache = Cache::open(dir.path(), &configured(), &mut facts, no_aside).unwrap();
        let good = advertised();
        cache.put(NodeId(1), &good).unwrap();
        let bad = dto::Offer {
            spec: "x".repeat(MAX_MESSAGE + 1),
            models: Vec::new(),
        };
        assert!(matches!(
            cache.put(NodeId(1), &bad),
            Err(Refusal::Exhausted { .. })
        ));
        assert_eq!(
            cache.files.all().unwrap()[0].1,
            serde_json::to_string(&good).unwrap()
        );
    }

    /// AC-T7.4.1/LB03: a record link must not redirect boot's file read outside the leased state.
    /// Do link peer 1's record to a valid offer outside; expect the link set aside unfollowed (it
    /// is removed, its target untouched) and peer 1 uncached.
    #[cfg(unix)]
    #[test]
    fn linked_cache_record_is_set_aside_unfollowed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let offer = serde_json::to_string(&advertised()).unwrap();
        fs::write(outside.path(), &offer).unwrap();
        let link = dir.path().join("peer-1.rec");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let mut facts = cluster();
        let before = facts.clone();
        assert_eq!(
            opened(dir.path(), &configured(), &mut facts).unwrap(),
            [SetAside {
                entry: "peer-1.rec".into(),
                why: Aside::NotAFile,
            }]
        );
        assert_eq!(facts, before);
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read_to_string(outside.path()).unwrap(), offer);
    }

    /// AC-T7.4.1/LB03, kept fail-closed: a folder in the cache is none this cache made, and
    /// removing a tree is not its to do. Do open with one beside peer 1's valid offer; expect the
    /// open refused, naming it, with no caller fact changed and nothing removed.
    #[test]
    fn a_folder_in_the_cache_refuses_the_open_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("peer-1.rec"),
            serde_json::to_string(&advertised()).unwrap(),
        )
        .unwrap();
        fs::create_dir(dir.path().join("kept")).unwrap();
        let mut facts = cluster();
        let before = facts.clone();
        assert!(matches!(
            Cache::open(dir.path(), &configured(), &mut facts, no_aside),
            Err(Refusal::Invalid { what: "peer offer cache", why }) if why.starts_with("kept is a folder")
        ));
        assert_eq!(facts, before);
        assert!(dir.path().join("peer-1.rec").is_file());
        assert!(dir.path().join("kept").is_dir());
    }

    /// AC-T7.4.1/LB03, kept fail-closed: the cache's own path must be a folder, or its writes would
    /// leave the leased state. Do open where it is a file, a link, or under a file; expect each
    /// refused (the last a storage error, which stays one), no caller fact changed.
    #[test]
    fn a_cache_path_that_is_no_folder_refuses_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let mut facts = cluster();
        let before = facts.clone();
        assert!(matches!(
            Cache::open(&file, &configured(), &mut facts, no_aside),
            Err(Refusal::Invalid {
                what: "peer offer cache",
                ..
            })
        ));
        assert!(matches!(
            Cache::open(&file.join("peers"), &configured(), &mut facts, no_aside),
            Err(Refusal::Broken(_))
        ));
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
            assert!(matches!(
                Cache::open(&link, &configured(), &mut facts, no_aside),
                Err(Refusal::Invalid {
                    what: "peer offer cache",
                    ..
                })
            ));
            assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
        }
        assert_eq!(facts, before);
    }
}
