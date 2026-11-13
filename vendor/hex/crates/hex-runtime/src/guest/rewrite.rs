//! A box's spec, as the host's caller wrote it, made one the guest's node can run (docs/design/
//! shards-guest.md §4a): its root (a bundle's `root.path`, a `Rootfs::Dir` or its layers) is copied into
//! the guest and named by its copy's path, and each bind mount's source is named by its path under the
//! share that holds it. A non-bind mount names no host path and passes as it is.

use std::path::{Path, PathBuf};

use hex_api::{BoxId, BoxSpec, ClassSpec, OciSpec, Refusal, Rootfs};
use hex_client::guest::stage::{
    Item, Layer, Source, blob_digest, box_layer_path, is_tarball, staged_path,
};
use serde_json::Value;

use super::shares::{self, Share};

// The most `config.json` a bundle's rewrite reads is the `oci` class's own, the one the guest's node
// parses it under, so no spec passes here that the guest refuses for its size.
use hex_core::bundle::MAX_CONFIG_BYTES;

/// A spec for the guest, and what to stage for it first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    pub spec: BoxSpec,
    pub items: Vec<Item>,
}

/// The digests the host learned for layer tarballs not named by theirs, by path, each trusted only while
/// its file's identity is the one it was learned with (the caller checks).
pub type Known = std::collections::BTreeMap<PathBuf, hex_core::stream::files::Digest>;

/// `spec` rewritten for a guest whose shares are `shared` and whose relay stages under `staged`, its layer
/// tarballs' digests their blob names' or `known`'s.
pub fn rewrite(
    spec: BoxSpec,
    shared: &[Share],
    staged: &str,
    known: &Known,
) -> Result<Rewritten, Refusal> {
    // Its owner goes with it: the guest's node hands a box's cgroup on within its owner, as any node does.
    let BoxSpec {
        id,
        bounds,
        class,
        owner,
        devices,
        allocation,
        route,
        band,
    } = spec;
    let mut items = Vec::new();
    let class = match class {
        ClassSpec::Process => ClassSpec::Process,
        class @ (ClassSpec::Oci(_) | ClassSpec::Shards(_)) => {
            let (mut oci, shards) = match class {
                ClassSpec::Oci(oci) => (oci, false),
                ClassSpec::Shards(oci) => (oci, true),
                _ => return Err(Refusal::Broken("guest class changed during rewrite".into())),
            };
            oci.rootfs = match oci.rootfs {
                Rootfs::Dir(dir) => {
                    Rootfs::Dir(stage(staged, &id, &mut items, "root".to_owned(), dir)?)
                }
                Rootfs::Layers(layers) => {
                    Rootfs::Layers(stage_layers(staged, &id, &mut items, layers, known)?)
                }
            };
            for mount in &mut oci.mounts {
                let guest = shares::in_guest(shared, &mount.source)
                    .ok_or_else(|| shares::unshared(shared, &mount.destination, &mount.source))?;
                mount.source = PathBuf::from(guest);
            }
            if shards {
                ClassSpec::Shards(oci)
            } else {
                ClassSpec::Oci(oci)
            }
        }
        ClassSpec::Bundle(dir) => {
            let (root, config) = bundle(&dir, shared)?;
            items.push(Item {
                name: "root".to_owned(),
                source: root_source(root)?,
            });
            items.push(Item {
                name: "config.json".to_owned(),
                source: Source::Bytes(config),
            });
            ClassSpec::Bundle(PathBuf::from(format!("{staged}/{id}")))
        }
        ClassSpec::Firecracker(_) => {
            return Err(Refusal::Invalid {
                what: "guest backend",
                why: "the Shards adapter cannot execute Firecracker work".into(),
            });
        }
        ClassSpec::ShardsImage(_) => {
            return Err(Refusal::Invalid {
                what: "guest backend",
                why: "a Shards image VM boots its image under shards-init, never a staged payload"
                    .into(),
            });
        }
    };
    Ok(Rewritten {
        spec: BoxSpec {
            id,
            bounds,
            class,
            owner,
            devices,
            allocation,
            route,
            band,
        },
        items,
    })
}

/// The spec's own refusals (HEX-RT-13), before its host paths are mapped or a guest is booted: every
/// value but the host paths as [`OciSpec::check`] reads it on any node, with each host path stood in by
/// `/`; and each host path as this host reads it (absolute here, UTF-8 and without a NUL, as the guest's
/// config carries it), since `check` reads them as a Linux host's and a Windows host's are not.
pub fn check(spec: &OciSpec) -> Result<(), Refusal> {
    let root = || PathBuf::from("/");
    let mut probe = spec.clone();
    probe.rootfs = match &spec.rootfs {
        Rootfs::Dir(_) => Rootfs::Dir(root()),
        Rootfs::Layers(layers) => Rootfs::Layers(layers.iter().map(|_| root()).collect()),
    };
    for mount in &mut probe.mounts {
        mount.source = root();
    }
    probe.check()?;
    let mut paths: Vec<(String, &Path)> = match &spec.rootfs {
        Rootfs::Dir(dir) => vec![("rootfs.dir".to_owned(), dir.as_path())],
        Rootfs::Layers(layers) => layers
            .iter()
            .enumerate()
            .map(|(i, layer)| (format!("rootfs.layers[{i}]"), layer.as_path()))
            .collect(),
    };
    paths.extend(
        spec.mounts
            .iter()
            .enumerate()
            .map(|(i, m)| (format!("mounts[{i}].source"), m.source.as_path())),
    );
    for (field, path) in paths {
        let invalid = |why: &str| Refusal::Invalid {
            what: "oci spec",
            why: format!("{field} {path:?} {why}"),
        };
        let text = path
            .to_str()
            .ok_or_else(|| invalid("isn't UTF-8, which config.json can't carry"))?;
        if text.contains('\0') {
            return Err(invalid("holds a NUL"));
        }
        if !path.is_absolute() {
            return Err(invalid("isn't an absolute path"));
        }
    }
    Ok(())
}

/// Adds the root at `path` to stage as `name`, and returns its copy's path in the guest.
fn stage(
    staged: &str,
    id: &BoxId,
    items: &mut Vec<Item>,
    name: String,
    path: PathBuf,
) -> Result<PathBuf, Refusal> {
    let guest = PathBuf::from(staged_path(staged, id, &name));
    items.push(Item {
        name,
        source: root_source(path)?,
    });
    Ok(guest)
}

/// Adds an image's layers to stage, and returns their paths in the guest. Tarballs are an image's own
/// layers, each applied as containerd's overlay applier applies one layer, so the guest mounts them as the
/// overlay's lower layers, as a Linux host's overlayfs snapshotter does: each in the guest's layer store by
/// its digest where the store has room, a layer the guest holds staged no more, else in the box (§4a). The
/// box names each `layers/<i>` either way. Directories are each staged as they are, an overlay's layers
/// already. A list of both is refused by name: a tarball's whiteouts would have to apply over a directory
/// staged apart from it.
fn stage_layers(
    staged: &str,
    id: &BoxId,
    items: &mut Vec<Item>,
    layers: Vec<PathBuf>,
    known: &Known,
) -> Result<Vec<PathBuf>, Refusal> {
    let tarballs = layers.iter().filter(|l| is_tarball(l)).count();
    if tarballs == 0 {
        return layers
            .into_iter()
            .enumerate()
            .map(|(i, layer)| stage(staged, id, items, format!("layer{i}"), layer))
            .collect();
    }
    if tarballs != layers.len() {
        return Err(Refusal::Invalid {
            what: "oci spec",
            why: "rootfs.layers mixes tarballs and directories; give an image's layers as one or \
                  the other"
                .to_owned(),
        });
    }
    // Each layer's digest is its blob's name, or one learned when it last crossed (`known`), never learned
    // by reading it here: a create costs a layer the guest holds its name alone. A layer of neither is
    // staged into the box, hashed as it crosses, for the next create to know (§4a).
    let layers: Vec<Layer> = layers
        .into_iter()
        .map(|path| Layer {
            digest: blob_digest(&path).or_else(|| known.get(&path).copied()),
            path,
        })
        .collect();
    let guests = (0..layers.len())
        .map(|i| PathBuf::from(box_layer_path(staged, id, i)))
        .collect();
    items.push(Item {
        name: "root".to_owned(),
        source: Source::Layers(layers),
    });
    Ok(guests)
}

/// What the root at `path` is staged from (§4a): a tarball's own entries (`.tar`, `.tar.gz`, `.tgz`), or
/// the directory as this host reads it. On Windows a directory root is refused by name: NTFS keeps no
/// Unix modes, and a Linux symbolic link unpacked there loses its target's text (Git Bash's tar on
/// jupiter made the lab's `bin/ls` a link to `C:\Program Files\Git\usr\bin\busybox`), so its copy
/// would be silently wrong (CLAUDE.md §3).
pub fn root_source(path: PathBuf) -> Result<Source, Refusal> {
    if is_tarball(&path) {
        return Ok(Source::Tar(path));
    }
    if cfg!(windows) {
        return Err(Refusal::Platform {
            what: "a Linux root as a Windows directory",
            platform: "windows",
            why: format!(
                "{} is a directory, and NTFS keeps neither a Linux root's modes nor its symbolic \
                 links' targets; give the root as a .tar or .tar.gz",
                path.display()
            ),
        });
    }
    Ok(Source::Path(path))
}

fn invalid(why: String) -> Refusal {
    Refusal::Invalid {
        what: "bundle",
        why,
    }
}

/// A bundle's root on the host, and its `config.json` rewritten: `root.path` the copy's (`root`, beside
/// the config in the staged bundle), each bind mount's source its path under its share.
fn bundle(dir: &Path, shared: &[Share]) -> Result<(PathBuf, Vec<u8>), Refusal> {
    use std::io::Read;
    let path = dir.join("config.json");
    let file = std::fs::File::open(&path)
        .map_err(|e| invalid(format!("reading {}: {e}", path.display())))?;
    let mut text = Vec::new();
    let past = u64::try_from(MAX_CONFIG_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(past)
        .read_to_end(&mut text)
        .map_err(|e| invalid(format!("reading {}: {e}", path.display())))?;
    if text.len() > MAX_CONFIG_BYTES {
        return Err(invalid(format!(
            "{} is over the {MAX_CONFIG_BYTES} bytes the runtime reads",
            path.display()
        )));
    }
    let mut config: Value = serde_json::from_slice(&text)
        .map_err(|e| invalid(format!("{} isn't JSON: {e}", path.display())))?;
    let root = config
        .pointer_mut("/root/path")
        .ok_or_else(|| invalid("its config.json names no root.path".to_owned()))?;
    let host_root = host(
        dir,
        root.as_str()
            .ok_or_else(|| invalid("its config.json's root.path isn't a string".to_owned()))?,
    );
    *root = Value::String("root".to_owned());
    if let Some(mounts) = config.get_mut("mounts").and_then(Value::as_array_mut) {
        for mount in mounts {
            if !is_bind(mount) {
                continue;
            }
            let destination = mount
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or("a mount")
                .to_owned();
            let Some(source) = mount.get_mut("source") else {
                return Err(invalid(format!("bind mount {destination} names no source")));
            };
            let host_source = host(
                dir,
                source.as_str().ok_or_else(|| {
                    invalid(format!("bind mount {destination}'s source isn't a string"))
                })?,
            );
            let guest = shares::in_guest(shared, &host_source)
                .ok_or_else(|| shares::unshared(shared, &destination, &host_source))?;
            *source = Value::String(guest);
        }
    }
    let bytes = serde_json::to_vec(&config)
        .map_err(|e| invalid(format!("writing its rewritten config.json: {e}")))?;
    Ok((host_root, bytes))
}

/// A path in a bundle's config as the host reads it: relative ones are the bundle's (runtime-spec,
/// config.md "root": "a relative path ... to the bundle").
fn host(dir: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_owned()
    } else {
        dir.join(path)
    }
}

/// Whether a config's mount is a bind: its type `bind`, or `bind` or `rbind` among its options
/// (runtime-spec config.md, "Mounts"; runc's and crun's reading).
fn is_bind(mount: &Value) -> bool {
    let typed = mount.get("type").and_then(Value::as_str) == Some("bind");
    let optioned = mount
        .get("options")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options
                .iter()
                .any(|o| matches!(o.as_str(), Some("bind" | "rbind")))
        });
    typed || optioned
}
