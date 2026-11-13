//! The guest runtime's tests (docs/design/shards-guest.md §1, §4a, §5; native plan step 5): the shared
//! roots read from `HEX_GUEST_SHARES` or this OS's default, never `/`; a box's spec rewritten to the
//! guest's paths, its root staged and each bind source under its share, one under none refused by
//! name (HEX-GUEST-16); and where Shards has no backend, the class refused by name before anything
//! boots, and one guest booted for every create after (HEX-GUEST-17).

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::rewrite::{Known, rewrite};
use super::shares::{self, Share};
use super::*;
use hex_api::{Bounds, Mount, Network, OciSpec, Rootfs};
use hex_client::guest::stage::Source;

/// The `i`th share, `dir`, where the guest's init mounts it.
fn share(i: usize, dir: &str, read_only: bool) -> Share {
    Share {
        dir: PathBuf::from(dir),
        read_only,
        at: format!("/run/hex/shares/{i}"),
    }
}

const STAGED: &str = "/run/hex/staged";

/// HEX-GUEST-16: read share lists as an operator writes them: expect each root, `:ro` or a following
/// `ro` making it read-only; a relative root, an empty list, `ro` first and a list past the bound
/// refused by name.
#[cfg(unix)]
#[test]
fn shared_roots_read_as_the_operator_writes_them() {
    let read = |s: &str| shares::parse(&OsString::from(s));
    assert_eq!(
        read("/Users:/data:ro:/srv").unwrap(),
        vec![
            share(0, "/Users", false),
            share(1, "/data", true),
            share(2, "/srv", false)
        ]
    );
    for bad in ["", "relative", "ro:/a"] {
        assert!(
            matches!(
                read(bad),
                Err(Refusal::Invalid {
                    what: shares::ENV,
                    ..
                })
            ),
            "{bad:?}"
        );
    }
    let many: Vec<String> = (0..=shares::MAX_SHARES).map(|i| format!("/r{i}")).collect();
    assert!(read(&many.join(":")).is_err());
}

/// HEX-GUEST-16: read this OS's default roots: expect Docker Desktop's list on macOS, the users and
/// temp folders on Windows, nothing on Linux, every one read-write and none the host's root.
#[test]
fn the_default_roots_are_docker_desktops_never_the_hosts_root() {
    let defaults = shares::defaults().unwrap();
    let dirs: Vec<&Path> = defaults.iter().map(|s| s.dir.as_path()).collect();
    if cfg!(target_os = "macos") {
        let want = ["/Users", "/Volumes", "/private", "/tmp", "/var/folders"];
        assert_eq!(dirs, want.map(Path::new));
    } else if cfg!(target_os = "windows") {
        // The users folder holds this user's profile; the temp folder is the one TMP or TEMP names.
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        let temp = PathBuf::from(
            std::env::var_os("TMP")
                .or_else(|| std::env::var_os("TEMP"))
                .unwrap(),
        );
        assert_eq!(dirs, [profile.parent().unwrap(), temp.as_path()]);
    } else {
        assert!(dirs.is_empty());
    }
    assert!(defaults.iter().all(|s| !s.read_only));
    assert!(
        dirs.iter().all(|d| d.parent().is_some()),
        "a default shares a root"
    );
}

/// HEX-GUEST-16: map sources under nested shares: expect each under the deepest share that holds it,
/// and a source escaping by `..`, or under no share, under none.
#[test]
fn a_source_maps_under_the_deepest_share_that_holds_it() {
    let shared = [share(0, "/Users", false), share(1, "/Users/ada/data", true)];
    assert_eq!(
        shares::in_guest(&shared, Path::new("/Users/bo/x")).as_deref(),
        Some("/run/hex/shares/0/bo/x")
    );
    assert_eq!(
        shares::in_guest(&shared, Path::new("/Users/ada/data/set")).as_deref(),
        Some("/run/hex/shares/1/set")
    );
    assert_eq!(
        shares::in_guest(&shared, Path::new("/Users")).as_deref(),
        Some("/run/hex/shares/0")
    );
    assert_eq!(shares::in_guest(&shared, Path::new("/Users/../etc")), None);
    assert_eq!(shares::in_guest(&shared, Path::new("/etc")), None);
}

pub(super) fn oci(rootfs: Rootfs, mounts: Vec<Mount>) -> BoxSpec {
    BoxSpec {
        devices: Vec::new(),
        allocation: None,
        route: None,
        id: BoxId::new("box-1").unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs,
            init: Program::of(&["sleep", "1"]),
            mounts,
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    }
}

fn mount(source: &str, destination: &str, read_only: bool) -> Mount {
    Mount {
        source: PathBuf::from(source),
        destination: destination.to_owned(),
        read_only,
    }
}

/// HEX-GUEST-16: rewrite an OciSpec with a root folder and with layers, and a writable and a read-only
/// bind: expect its root named by the staged copy and staged from the host path (an image's layer blobs
/// named by the digests their names give, never read; docker save's `<id>/layer.tar` by a digest learned
/// before, or none; each layer the box's `layers/<i>`; tarballs mixed with directories refused), each
/// bind's source
/// its path under its share (writable binds pass), and a bind under no share refused naming it and the
/// shared roots.
#[test]
fn an_oci_spec_is_rewritten_to_the_guests_paths() {
    let shared = [share(0, "/Users", false), share(1, "/etc", false)];
    let got = rewrite(
        oci(
            Rootfs::Dir(PathBuf::from("/Users/ada/rootfs.tar")),
            vec![
                mount("/etc", "/host-etc", false),
                mount("/Users/ada/src", "/src", true),
            ],
        ),
        &shared,
        STAGED,
        &Known::new(),
    )
    .unwrap();
    let ClassSpec::Oci(spec) = &got.spec.class else {
        panic!("not oci");
    };
    assert_eq!(
        spec.rootfs,
        Rootfs::Dir(PathBuf::from("/run/hex/staged/box-1/root"))
    );
    assert_eq!(spec.mounts[0].source, PathBuf::from("/run/hex/shares/1"));
    assert!(!spec.mounts[0].read_only);
    assert_eq!(
        spec.mounts[1].source,
        PathBuf::from("/run/hex/shares/0/ada/src")
    );
    assert_eq!(got.items.len(), 1);
    assert_eq!(got.items[0].name, "root");
    assert_eq!(
        got.items[0].source,
        Source::Tar(PathBuf::from("/Users/ada/rootfs.tar"))
    );

    // Two layers as an image keeps them, each blob named by its digest (an OCI image layout's
    // blobs/sha256/<hex>, as containerd's content store's). Neither file exists: a layer's digest is its
    // name, never read to learn it (§4a; the lead's rule, 2026-11-06: a create never hashes a layer).
    let digests: Vec<[u8; 32]> = [[0xa0u8; 32], [0x1bu8; 32]].to_vec();
    let blobs: Vec<PathBuf> = digests
        .iter()
        .map(|d| {
            PathBuf::from(format!(
                "/Users/ada/image/blobs/sha256/{}",
                hex_core::stream::files::digest_hex(d)
            ))
        })
        .collect();
    // And a layer as `docker save` writes it, `<id>/layer.tar`, its name no digest: whole-root staging took
    // it, and so does the store (the lead's check, 2026-11-06), named by a digest learned when it last
    // crossed (`known`), or else by none, staged into the box and hashed as it crosses.
    let saved = PathBuf::from("/Users/ada/save/0f3e/layer.tar");
    let mut layers = blobs.clone();
    layers.push(saved.clone());
    let layered = rewrite(
        oci(Rootfs::Layers(layers.clone()), vec![]),
        &shared,
        STAGED,
        &Known::new(),
    )
    .unwrap();
    let ClassSpec::Oci(spec) = &layered.spec.class else {
        panic!("not oci");
    };
    // Each is a lower layer of the box's overlay, by the box's own name for it, lowest first.
    assert_eq!(
        spec.rootfs,
        Rootfs::Layers(
            (0..3)
                .map(|i| PathBuf::from(format!("{STAGED}/box-1/layers/{i}")))
                .collect()
        )
    );
    let named = |saved: Option<[u8; 32]>| {
        Source::Layers(
            layers
                .iter()
                .zip(digests.iter().copied().map(Some).chain([saved]))
                .map(|(path, digest)| hex_client::guest::stage::Layer {
                    path: path.clone(),
                    digest,
                })
                .collect(),
        )
    };
    assert_eq!(layered.items[0].source, named(None));
    let learned = [0x5au8; 32];
    let known: Known = [(saved.clone(), learned)].into_iter().collect();
    let again = rewrite(
        oci(Rootfs::Layers(layers.clone()), vec![]),
        &shared,
        STAGED,
        &known,
    )
    .unwrap();
    assert_eq!(again.items[0].source, named(Some(learned)));
    let Err(Refusal::Invalid { why, .. }) = rewrite(
        oci(
            Rootfs::Layers(vec![
                PathBuf::from("/Users/l0.tar"),
                PathBuf::from("/Users/l1"),
            ]),
            vec![],
        ),
        &shared,
        STAGED,
        &Known::new(),
    ) else {
        panic!("expected tarballs and directories together refused");
    };
    assert!(why.contains("mixes tarballs and directories"), "{why}");

    let Err(Refusal::Platform { why, .. }) = rewrite(
        oci(
            Rootfs::Dir(PathBuf::from("/Users/r.tar")),
            vec![mount("/opt/x", "/x", true)],
        ),
        &shared,
        STAGED,
        &Known::new(),
    ) else {
        panic!("expected the unshared bind refused");
    };
    assert!(
        why.contains("/x's source /opt/x") && why.contains("[/Users, /etc]"),
        "{why}"
    );
}

/// HEX-GUEST-16 (§4a; the lead's conditions, 2026-11-06): learn the digests of three layer files from a
/// stage whose guest's store holds two layers: expect the least recently learned forgotten, and the other
/// two known by path while their files are as they were read. Rewrite one (its identity changes): expect it
/// unknown, so it's hashed again when it next crosses, never trusted.
#[test]
fn a_learned_digest_is_trusted_while_its_file_is_the_same_within_what_the_store_holds() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<PathBuf> = ["a", "b", "c"]
        .iter()
        .map(|n| {
            let path = dir.path().join(n).join("layer.tar");
            std::fs::create_dir(path.parent().unwrap()).unwrap();
            std::fs::write(&path, n.as_bytes()).unwrap();
            path
        })
        .collect();
    let learned = paths
        .iter()
        .enumerate()
        .map(|(i, path)| hex_client::guest::stage::Learned {
            path: path.clone(),
            digest: [u8::try_from(i).unwrap(); 32],
            identity: hex_client::guest::stage::identity(&std::fs::metadata(path).unwrap()),
        })
        .collect();
    let mut digests = Digests::default();
    digests.learn(hex_client::guest::stage::Staged {
        learned,
        capacity: 2,
    });
    let known = digests.known(&paths);
    assert_eq!(
        known,
        [(paths[1].clone(), [1; 32]), (paths[2].clone(), [2; 32])]
            .into_iter()
            .collect::<Known>()
    );
    std::fs::write(&paths[2], b"changed").unwrap();
    assert!(
        digests.known(&paths[2..]).is_empty(),
        "a changed file is never trusted"
    );
}

/// HEX-GUEST-16: rewrite a bundle whose config names a relative root, a relative and an absolute
/// writable bind (both kinds of bind option) and a proc mount: expect the bundle the staged folder,
/// its root staged from the bundle's, its config's root `root`, each bind's source its share path and
/// the proc mount untouched; and a bind under no share refused.
#[cfg(unix)]
#[test]
fn a_bundles_config_is_rewritten_to_the_guests_paths() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir_all(bundle.join("rootfs")).unwrap();
    let base = dir.path().to_str().unwrap().to_owned();
    let config = serde_json::json!({
        "ociVersion": "1.2.0",
        "root": {"path": "rootfs"},
        "mounts": [
            {"destination": "/proc", "type": "proc", "source": "proc"},
            {"destination": "/etc-host", "type": "bind", "source": "/etc", "options": ["rbind", "rw"]},
            {"destination": "/data", "source": "data", "options": ["bind"]},
        ],
    });
    std::fs::write(bundle.join("config.json"), config.to_string()).unwrap();
    let shared = [share(0, "/etc", false), share(1, &base, false)];
    let spec = BoxSpec {
        devices: Vec::new(),
        allocation: None,
        route: None,
        id: BoxId::new("box-1").unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle.clone()),
        owner: None,
        band: 0,
    };
    let got = rewrite(spec.clone(), &shared, STAGED, &Known::new()).unwrap();
    assert_eq!(
        got.spec.class,
        ClassSpec::Bundle(PathBuf::from("/run/hex/staged/box-1"))
    );
    assert_eq!(got.items[0].source, Source::Path(bundle.join("rootfs")));
    let Source::Bytes(bytes) = &got.items[1].source else {
        panic!("config.json isn't bytes");
    };
    assert_eq!(got.items[1].name, "config.json");
    let rewritten: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(rewritten["root"]["path"], "root");
    assert_eq!(rewritten["mounts"][0]["source"], "proc");
    assert_eq!(rewritten["mounts"][1]["source"], "/run/hex/shares/0");
    assert_eq!(
        rewritten["mounts"][2]["source"],
        "/run/hex/shares/1/bundle/data"
    );
    assert!(matches!(
        rewrite(spec, &[share(0, "/etc", false)], STAGED, &Known::new()),
        Err(Refusal::Platform { .. })
    ));
}

/// A stand-in for Shards: a backend or the refusal of one, counting boots, each refused as a boot
/// failure (no guest is needed to show none was asked for).
struct NoGuest {
    backend: Result<shards_api::Backend, shards_api::Refusal>,
    boots: AtomicUsize,
}

impl shards_api::Guests for NoGuest {
    fn probe_gpu(
        &self,
        _: &shards_api::GuestSpec,
        _: Duration,
    ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
        Err(shards_api::Refusal::GpuProbe(
            shards_api::GpuProbeError::Unavailable,
        ))
    }

    fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
        self.backend.clone()
    }

    fn boot(
        &self,
        _spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.boots.fetch_add(1, Ordering::SeqCst);
        Err(shards_api::Refusal::Boot(
            "a stand-in boots nothing".to_owned(),
        ))
    }

    fn attach(
        &self,
        identity: &shards_api::Identity,
        _spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        Err(shards_api::Refusal::Gone {
            pid: identity.pid,
            why: "a stand-in boots no guest to take back".to_owned(),
        })
    }
}

pub(super) fn config() -> GuestConfig {
    GuestConfig {
        binary: PathBuf::from("/shards"),
        kernel: PathBuf::from("/k"),
        initrd: PathBuf::from("/i"),
        // Shards' least guest: one vCPU and its architecture's least memory (`shards_api::check`).
        vcpus: 1,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        gpu: false,
        shares: vec![share(0, "/", false)],
        staged: STAGED.to_owned(),
    }
}

// A directory for controls that reach the native model callback. Its prepared
// socket uses the platform's 103-byte minimum sun_path bound; a cohort's long
// TMPDIR is not a valid socket fixture. TempDir still owns/removes this directory.
pub(super) fn socket_fixture() -> tempfile::TempDir {
    #[cfg(unix)]
    return tempfile::tempdir_in("/tmp").unwrap();
    #[cfg(not(unix))]
    tempfile::tempdir().unwrap()
}

/// HEX-GUEST-17: create an oci box, its root an absolute path on this host, where Shards has no backend
/// (macOS x86_64's refusal): expect the class refused as the platform's, naming the OS, arch and Shards, and no boot asked for; a process
/// box refused as the host's; and a call about a box before any boot NoSuchBox, with nothing booted.
#[test]
fn where_shards_has_no_backend_the_class_is_refused_before_anything_boots() {
    let guests = Arc::new(NoGuest {
        backend: Err(shards_api::Refusal::NoBackend {
            os: "macos",
            arch: "x86_64",
        }),
        boots: AtomicUsize::new(0),
    });
    let runtime = GuestRuntime::new(guests.clone(), config());
    // An absolute path on this host (a drive's on Windows): the spec's own check passes it, so the
    // refusal that comes is the platform's.
    let host = tempfile::tempdir().unwrap();
    let got = runtime.create(oci(Rootfs::Dir(host.path().join("rootfs.tar")), vec![]));
    let Err(Refusal::Platform { platform, why, .. }) = got else {
        panic!("expected the platform's refusal, got {got:?}");
    };
    assert_eq!(platform, "macos");
    assert!(why.contains("x86_64") && why.contains("Shards"), "{why}");
    assert_eq!(guests.boots.load(Ordering::SeqCst), 0);
    let process = BoxSpec {
        devices: Vec::new(),
        allocation: None,
        route: None,
        id: BoxId::new("p").unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    };
    assert!(matches!(
        runtime.create(process),
        Err(Refusal::Invalid { .. })
    ));
    let id = BoxId::new("box-1").unwrap();
    assert_eq!(runtime.inspect(&id), Err(Refusal::NoSuchBox(id.clone())));
    assert_eq!(runtime.list(), Ok(Vec::new()));
    assert_eq!(guests.boots.load(Ordering::SeqCst), 0);
}

/// HEX-GUEST-17: read the guest's spec from its config: expect the initramfs as its init, the init's
/// command line naming the shares, the channel's three ports alone, and each share as configured.
#[test]
fn the_guests_spec_names_its_init_ports_and_shares() {
    let spec = config().spec();
    assert_eq!(spec.cmdline, "console=ttyS0 panic=-1 hex.shares=rw");
    let none = GuestConfig {
        shares: Vec::new(),
        ..config()
    };
    assert_eq!(none.spec().cmdline, "console=ttyS0 panic=-1");
    assert_eq!(
        spec.ports,
        [stream::CONTROL_PORT, stream::STDIO_PORT, files::FILES_PORT]
    );
    assert_eq!(spec.shares.len(), 1);
    assert!(spec.disks.is_empty());
    assert!(!spec.gpu);
    let gpu = GuestConfig {
        gpu: true,
        ..config()
    }
    .spec();
    assert!(gpu.gpu);
    assert_eq!(gpu.cmdline, spec.cmdline);
    assert_eq!(gpu.ports, spec.ports);
    assert_eq!(gpu.shares, spec.shares);
}

/// HEX-GUEST-17: read the guest from variables with Shards' binary, the kernel and the initramfs
/// missing, then with every one missing: expect one refusal naming each missing one, read before
/// anything boots; then with all given: expect the guest they name, booted with that binary.
#[test]
fn every_missing_guest_input_is_named_in_one_refusal() {
    use std::collections::BTreeMap;
    let read = |given: &[(&str, &str)]| {
        let vars: BTreeMap<String, OsString> = given
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        GuestConfig::from_vars(move |name| vars.get(name).cloned())
    };
    // Inputs to the parse, not a guest's size: any numbers do.
    let sized = [("HEX_GUEST_CPUS", "2"), ("HEX_GUEST_MEMORY_MIB", "1024")];
    let Err(Refusal::Invalid { why, .. }) = read(&sized) else {
        panic!("expected a refusal");
    };
    assert!(
        why.starts_with("SHARDS_BIN, HEX_GUEST_KERNEL, HEX_GUEST_INITRD unset"),
        "{why}"
    );
    let Err(Refusal::Invalid { why, .. }) = read(&[("SHARDS_BIN", "")]) else {
        panic!("expected a refusal");
    };
    assert!(
        why.starts_with(
            "SHARDS_BIN, HEX_GUEST_KERNEL, HEX_GUEST_INITRD, HEX_GUEST_CPUS, HEX_GUEST_MEMORY_MIB unset"
        ),
        "{why}"
    );
    let mut all = sized.to_vec();
    all.extend([
        ("SHARDS_BIN", "/opt/shards"),
        ("HEX_GUEST_KERNEL", "/k"),
        ("HEX_GUEST_INITRD", "/i"),
        (
            "HEX_GUEST_SHARES",
            if cfg!(windows) { r"C:\data" } else { "/data" },
        ),
    ]);
    let config = read(&all).unwrap();
    assert_eq!((config.vcpus, config.memory_mib), (2, 1024));
    assert_eq!(config.spec().binary, Some(PathBuf::from("/opt/shards")));
    assert_eq!(config.shares.len(), 1);
    assert!(!config.gpu);
    for (value, enabled) in [("false", false), ("true", true)] {
        let mut vars = all.clone();
        vars.push((ENV_GPU, value));
        let selected = read(&vars).unwrap();
        assert_eq!(selected.spec().gpu, enabled);
        assert_eq!(selected.spec().ports, config.spec().ports);
    }
    for value in ["", "TRUE", "1", "yes"] {
        let mut vars = all.clone();
        vars.push((ENV_GPU, value));
        assert!(matches!(
            read(&vars),
            Err(Refusal::Invalid { what: ENV_GPU, .. })
        ));
    }
}

/// AC-T7.4.1, joint plan step 39 (original; docs/design/node-under-shards-init.md §4, §5): a node's guest under
/// shards-init is named by `HEX_GUEST_IMAGE`, an Agentfile image of Hex's own init. Its host reaches that guest's node
/// only through a lane of Shards' agent-control endpoint to the node's sockets, which Shards doesn't give yet. Do:
/// read the guest from variables naming the image, alone and then beside the direct boot's kernel and initramfs.
/// Expect: alone, refused `Awaits(NestedChannel)` before anything boots, not as a direct boot missing its kernel;
/// beside them, refused naming both forms, as a node boots one form of its guest.
#[test]
fn original_a_guest_under_shards_init_awaits_its_channel_before_anything_boots() {
    use std::collections::BTreeMap;
    let read = |given: &[(&str, &str)]| {
        let vars: BTreeMap<String, OsString> = given
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        GuestConfig::from_vars(move |name| vars.get(name).cloned())
    };
    // Inputs to the parse, not a guest's size or an image: any values do.
    let image = [
        ("SHARDS_BIN", "/opt/shards"),
        ("HEX_GUEST_IMAGE", "hex-node@sha256:0123"),
        ("HEX_GUEST_CPUS", "2"),
        ("HEX_GUEST_MEMORY_MIB", "1024"),
    ];
    assert_eq!(
        read(&image),
        Err(Refusal::Awaits {
            needs: hex_api::Upstream::NestedChannel
        })
    );
    let mut both = image.to_vec();
    both.extend([("HEX_GUEST_KERNEL", "/k"), ("HEX_GUEST_INITRD", "/i")]);
    let Err(Refusal::Invalid { why, .. }) = read(&both) else {
        panic!("expected a refusal naming both forms");
    };
    for name in ["HEX_GUEST_IMAGE", "HEX_GUEST_KERNEL", "HEX_GUEST_INITRD"] {
        assert!(why.contains(name), "{name}: {why}");
    }
}

/// One value of a spec broken, as HEX-RT-13 breaks them.
type Break = fn(&mut OciSpec);

/// HEX-RT-13 through the guest (found by HEX-GUEST-19 on studio-2): create oci boxes whose specs break
/// one value each (HEX-RT-13's), their bind source under no share: expect each refused as the spec's
/// own Invalid naming the field, as on any node, before its sources are mapped to shares and before
/// anything boots.
#[test]
fn an_unrenderable_spec_is_refused_by_its_field_before_its_shares_or_a_boot() {
    let guests = Arc::new(NoGuest {
        backend: Ok(shards_api::Backend::Kvm),
        boots: AtomicUsize::new(0),
    });
    let runtime = GuestRuntime::new(guests.clone(), config());
    let unshared = GuestConfig {
        shares: vec![share(0, "/Users", false)],
        ..config()
    };
    let unshared = GuestRuntime::new(guests.clone(), unshared);
    let broken: [(&str, Break); 3] = [
        ("mounts[0].destination", |s| {
            s.mounts[0].destination = "data".to_owned();
        }),
        ("hostname", |s| s.hostname = Some("two words".to_owned())),
        ("init.cwd", |s| s.init.cwd = Some("relative".to_owned())),
    ];
    for (field, break_it) in broken {
        let mut spec = oci(
            Rootfs::Dir(PathBuf::from("/hex-conformance/rootfs")),
            vec![mount("/hex-conformance/data", "/data", true)],
        );
        let ClassSpec::Oci(o) = &mut spec.class else {
            panic!("not oci");
        };
        break_it(o);
        for rt in [&runtime, &unshared] {
            let got = rt.create(spec.clone());
            assert!(
                matches!(&got, Err(Refusal::Invalid { what: "oci spec", why }) if why.contains(field)),
                "{field}: {got:?}"
            );
        }
    }
    assert_eq!(guests.boots.load(Ordering::SeqCst), 0);
}

/// HEX-GUEST-16 (§4a): rewrite an OciSpec whose root is a tarball, and one whose root is a directory: expect
/// the tarball staged from its own entries everywhere, and the directory staged as it is on macOS and
/// Linux but refused by name on Windows, whose NTFS keeps neither its modes nor its links' texts.
#[test]
fn a_tarball_root_stages_everywhere_and_a_directory_root_is_refused_on_windows() {
    let host = tempfile::tempdir().unwrap();
    let tarball = host.path().join("rootfs.tar.gz");
    let got = rewrite(
        oci(Rootfs::Dir(tarball.clone()), vec![]),
        &[],
        STAGED,
        &Known::new(),
    )
    .unwrap();
    assert_eq!(got.items[0].source, Source::Tar(tarball));
    let dir = host.path().join("rootfs");
    let got = rewrite(
        oci(Rootfs::Dir(dir.clone()), vec![]),
        &[],
        STAGED,
        &Known::new(),
    );
    if cfg!(windows) {
        let Err(Refusal::Platform { why, .. }) = got else {
            panic!("expected a Windows directory root refused, got {got:?}");
        };
        assert!(why.contains(".tar or .tar.gz"), "{why}");
    } else {
        assert_eq!(got.unwrap().items[0].source, Source::Path(dir));
    }
}

/// How a stand-in guest answers its first dial.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Boots {
    /// Its relay answers: the token, then the node's hello.
    Ready,
    /// Alive, its relay never listening: the dial's patience runs out (the hung-guest tests, which need a
    /// Shards backend: macOS and Windows).
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Hung,
}

/// A stand-in for Shards whose guests boot as `plan` says, one entry a boot, recording each first
/// dial's patience, each absolute deadline a dial carries, when each boot was asked, and each stop.
struct Planned {
    plan: Mutex<Vec<Boots>>,
    patience: Arc<Mutex<Vec<Duration>>>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
    booted: Mutex<Vec<Instant>>,
    stops: Arc<AtomicUsize>,
    boots: AtomicUsize,
    /// Each boot's memory, in MiB.
    memory: Mutex<Vec<u64>>,
    /// Each boot's kernel command line.
    cmdlines: Mutex<Vec<String>>,
}

impl Planned {
    fn new(plan: &[Boots]) -> Self {
        Self {
            plan: Mutex::new(plan.iter().rev().copied().collect()),
            patience: Arc::new(Mutex::new(Vec::new())),
            deadlines: Arc::new(Mutex::new(Vec::new())),
            booted: Mutex::new(Vec::new()),
            stops: Arc::new(AtomicUsize::new(0)),
            boots: AtomicUsize::new(0),
            memory: Mutex::new(Vec::new()),
            cmdlines: Mutex::new(Vec::new()),
        }
    }
}

/// The boot's deadline `cmdline`, a guest's kernel command line, names, once (`hex.ready-ms=`).
pub(super) fn ready_ms(cmdline: &str) -> Duration {
    let named: Vec<u64> = cmdline
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("hex.ready-ms="))
        .map(|ms| ms.parse().unwrap())
        .collect();
    let [ms] = named.as_slice() else {
        panic!(
            "{cmdline:?} names the boot's deadline (hex.ready-ms) {} times, not once",
            named.len()
        )
    };
    Duration::from_millis(*ms)
}

struct PlannedGuest {
    boots: Boots,
    patience: Arc<Mutex<Vec<Duration>>>,
    deadlines: Arc<Mutex<Vec<Instant>>>,
    stops: Arc<AtomicUsize>,
}

/// A loopback stream as Shards' guest stream.
struct Looped(std::net::TcpStream);

impl std::io::Read for Looped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for Looped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl shards_api::Stream for Looped {
    fn try_clone(&self) -> std::io::Result<Box<dyn shards_api::Stream>> {
        Ok(Box::new(Looped(self.0.try_clone()?)))
    }

    fn handle(&self) -> shards_api::Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.0);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.0)
    }

    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        self.0.set_nonblocking(on)
    }

    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.0.shutdown(how)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    /// Not asked of a guest's stream by Hex: refused, as loopback's are std's to keep.
    fn buffer_sizes(&self) -> std::io::Result<(usize, usize)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "a test's loopback stream reports no buffer sizes",
        ))
    }
}

/// A relay and node agent in one: the connection's token, then the answer to its hello.
fn ready_relay() -> std::net::TcpStream {
    ready_relay_with_answer(None)
}

fn ready_relay_with_answer(answer: Option<hex_core::wire::Answer>) -> std::net::TcpStream {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.write_all(&7u64.to_le_bytes()).unwrap();
        let mut d = stream::Deframer::new(hex_core::wire::MAX_MESSAGE);
        let mut byte = [0u8; 1];
        let message = loop {
            conn.read_exact(&mut byte).unwrap();
            if let (_, Some(m)) = d.take(&byte).unwrap() {
                break m;
            }
        };
        let (h, _) =
            hex_core::wire::read_request_apart(&message, hex_core::wire::MAX_MESSAGE).unwrap();
        let hello = hex_core::wire::Answer::Hello(hex_core::wire::Hello {
            build: hex_core::wire::build(),
            classes: vec![hex_api::Class::Oci],
            limits: hex_core::wire::Limits {
                connections: 1,
                message_bytes: hex_core::wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 1,
            },
            budgets: hex_core::wire::Budgets {
                in_flight: 1,
                programs: 1,
            },
        });
        let bytes = hex_core::wire::answer(h.id, &hello, hex_core::wire::MAX_MESSAGE).unwrap();
        conn.write_all(&stream::frame(&bytes, hex_core::wire::MAX_MESSAGE).unwrap())
            .unwrap();
        if let Some(answer) = answer {
            let message = loop {
                conn.read_exact(&mut byte).unwrap();
                if let (_, Some(m)) = d.take(&byte).unwrap() {
                    break m;
                }
            };
            let (header, request) =
                hex_core::wire::read_request_apart(&message, hex_core::wire::MAX_MESSAGE).unwrap();
            assert_eq!(
                request,
                hex_core::wire::Request::Call(hex_core::wire::Call::Capacity(Class::Oci))
            );
            let bytes =
                hex_core::wire::answer(header.id, &answer, hex_core::wire::MAX_MESSAGE).unwrap();
            conn.write_all(&stream::frame(&bytes, hex_core::wire::MAX_MESSAGE).unwrap())
                .unwrap();
        }
        // Held until the client goes, as a relay's connection is.
        let _ = conn.read(&mut byte);
    });
    std::net::TcpStream::connect(addr).unwrap()
}

impl shards_api::Guest for PlannedGuest {
    fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
        Ok(false)
    }

    fn connect(
        &self,
        _port: u32,
        patience: Duration,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.patience.lock().unwrap().push(patience);
        match self.boots {
            Boots::Ready => Ok(Box::new(Looped(ready_relay()))),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Boots::Hung => Err(shards_api::Refusal::Channel {
                port: _port,
                why: format!("unanswered in {patience:?}"),
            }),
        }
    }

    fn connect_before(
        &self,
        port: u32,
        at: std::time::Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        self.deadlines.lock().unwrap().push(at);
        let left = at
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::Dial,
                },
            })?;
        let stream = self.connect(port, left)?;
        if std::time::Instant::now() >= at {
            return Err(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::ClearTimeouts,
                },
            });
        }
        Ok(stream)
    }

    fn stop(&self) -> Result<(), shards_api::Refusal> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
        Ok(None)
    }

    fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
        Err(shards_api::Refusal::Unsupported {
            what: "identity",
            why: "a planned stand-in names no VM".into(),
        })
    }
}

impl shards_api::Guests for Planned {
    fn probe_gpu(
        &self,
        _: &shards_api::GuestSpec,
        _: Duration,
    ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
        Err(shards_api::Refusal::GpuProbe(
            shards_api::GpuProbeError::Unavailable,
        ))
    }

    fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
        shards_api::backend()
    }

    fn boot(
        &self,
        spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.booted.lock().unwrap().push(Instant::now());
        self.boots.fetch_add(1, Ordering::SeqCst);
        self.memory.lock().unwrap().push(spec.memory_mib);
        self.cmdlines.lock().unwrap().push(spec.cmdline.clone());
        let boots = self.plan.lock().unwrap().pop().unwrap();
        Ok(Box::new(PlannedGuest {
            boots,
            patience: Arc::clone(&self.patience),
            deadlines: Arc::clone(&self.deadlines),
            stops: Arc::clone(&self.stops),
        }))
    }

    fn attach(
        &self,
        identity: &shards_api::Identity,
        _spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        Err(shards_api::Refusal::Gone {
            pid: identity.pid,
            why: "a stand-in boots no guest to take back".to_owned(),
        })
    }
}

/// HEX-GUEST-34, AC-T7.4.1 (CLAUDE.md §2): boot a guest: expect its first dial to reach Shards as
/// `connect_before` with the boot's own deadline, its bound counted from before the boot, so a boot that spends
/// the bound is refused by type at that dial; never a patience counted down after the boot, which such a boot
/// leaves at zero and Shards' client then awaits without a bound.
#[test]
fn the_first_dial_spends_the_boots_own_deadline() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    let bound = rt.first_dial_bound().unwrap();
    let asked = Instant::now();
    rt.guest().unwrap();
    let booted = guests.booted.lock().unwrap().clone();
    let deadlines = guests.deadlines.lock().unwrap().clone();
    let patience = guests.patience.lock().unwrap().clone();
    eprintln!(
        "hex_guest_34_first_dial bound={bound:?} asked={asked:?} booted={booted:?} deadlines={deadlines:?} patience={patience:?}"
    );
    assert_eq!(booted.len(), 1);
    assert_eq!(
        deadlines.len(),
        1,
        "the first dial was given a patience counted after the boot, {patience:?}, not the boot's deadline"
    );
    assert!(
        asked + bound <= deadlines[0] && deadlines[0] <= booted[0] + bound,
        "deadline {:?} is not the bound from before the boot, [{:?}, {:?}]",
        deadlines[0],
        asked + bound,
        booted[0] + bound
    );
}

/// AC-T7.4.1 (original): on 00aad a guest's relay was refused its node's hello at the hello's learned bound, the node
/// busy initializing the guest's GPU, while the boot had most of its deadline left, and the guest panicked. A
/// Firecracker guest is told its boot's deadline (`hex.firecracker-ready-ms`), which its relay's hello takes as its
/// caller's; a Shards guest is told none. Do: boot a guest. Expect its kernel command line to name the boot's deadline
/// once (`hex.ready-ms=`): what is left of the first dial's bound as it boots, in milliseconds rounded up, so that,
/// counted from the boot, it ends no sooner than the host's.
#[test]
fn original_a_shards_boot_tells_its_guest_the_boots_deadline() {
    if shards_api::backend().is_err() {
        eprintln!("SKIP: no Shards backend on this host");
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    let bound = rt.first_dial_bound().unwrap();
    let asked = Instant::now();
    rt.guest().unwrap();
    let cmdlines = guests.cmdlines.lock().unwrap().clone();
    let booted = guests.booted.lock().unwrap().clone();
    eprintln!("hex_guest_ready_word boot=cpu bound={bound:?} cmdlines={cmdlines:?}");
    assert_eq!(cmdlines.len(), 1);
    let given = ready_ms(&cmdlines[0]);
    assert!(
        given <= bound + Duration::from_millis(1),
        "{given:?} is past the boot's bound {bound:?}"
    );
    assert!(
        booted[0] + given >= asked + bound,
        "{given:?} from the boot ends before the host's deadline, {bound:?} from before it"
    );
}

/// HEX-GUEST-34, AC-T7.4.1 (CLAUDE.md §2, §3): learn a boot so slow that it is past any monotonic deadline,
/// then ask for the guest: expect it refused by type before anything boots or dials, never a dial handed a
/// patience that Shards' client reads as no clock.
#[test]
fn a_boot_bound_past_any_deadline_is_refused_before_a_boot() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    rt.learn(Duration::MAX);
    let refused = rt.guest().map(drop);
    let patience = guests.patience.lock().unwrap().clone();
    let deadlines = guests.deadlines.lock().unwrap().clone();
    eprintln!(
        "hex_guest_34_unbounded refused={refused:?} boots={} patience={patience:?} deadlines={deadlines:?}",
        guests.boots.load(Ordering::SeqCst)
    );
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        0,
        "booted, and dialed with patience {patience:?}, deadlines {deadlines:?}"
    );
    assert!(patience.is_empty() && deadlines.is_empty());
    assert!(
        matches!(
            refused,
            Err(Refusal::Invalid {
                what: "the guest's boot bound",
                ..
            })
        ),
        "{refused:?}"
    );
}

/// HEX-GUEST-34 (decision 9: a bound is the slowest answer on record, with no margin): boot a guest before any boot
/// is seen: expect its first dial bounded by the record's slowest boot to ready, what is left of it after the boot
/// call's own time, never unbounded.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn the_first_boot_is_bounded_by_the_record() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    let bound = rt.first_dial_bound().unwrap();
    assert_eq!(bound, recorded_boot().unwrap());
    let asked = Instant::now();
    rt.guest().unwrap();
    let spent = asked.elapsed();
    let given = guests.patience.lock().unwrap()[0];
    assert!(
        given <= bound && given + spent >= bound,
        "{given:?} of {bound:?}, {spent:?} spent"
    );
}

/// HEX-GUEST-34: tell the runtime of a boot slower than the record, then a faster one, and boot: expect
/// the first dial bounded by the slowest seen, the bound only rising.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn the_bound_rises_with_a_slow_boot_and_never_falls() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    let slow = 3 * recorded_boot().unwrap();
    rt.learn(slow);
    rt.learn(recorded_boot().unwrap() / 2);
    assert_eq!(rt.first_dial_bound().unwrap(), slow);
    let asked = Instant::now();
    rt.guest().unwrap();
    let spent = asked.elapsed();
    let given = guests.patience.lock().unwrap()[0];
    assert!(
        given <= slow && given + spent >= slow,
        "{given:?} of {slow:?}, {spent:?} spent"
    );
}

/// HEX-GUEST-34 (no caller fails where an alternative exists): boot a guest that stays alive and never
/// listens, then one that does: expect the first stopped and counted, the second booted and used.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn a_hung_guest_is_retired_and_booted_again() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Hung, Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), config());
    rt.guest().unwrap();
    assert_eq!(guests.boots.load(Ordering::SeqCst), 2);
    assert_eq!(guests.stops.load(Ordering::SeqCst), 1);
    assert_eq!(rt.retired(), 1);
}

/// HEX-GUEST-34: boot two guests that both hang: expect the caller refused by type, naming the boot and
/// its bound, after exactly two boots, both stopped.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn a_second_hang_is_refused_by_type() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Hung, Boots::Hung]));
    let rt = GuestRuntime::new(guests.clone(), config());
    let Err(Refusal::Os { op, why }) = rt.guest().map(|_| ()) else {
        panic!("expected the boot refused");
    };
    assert_eq!(op, "booting the node's guest");
    assert!(why.contains("ready"), "{why}");
    assert_eq!(guests.boots.load(Ordering::SeqCst), 2);
    assert_eq!(guests.stops.load(Ordering::SeqCst), 2);
    assert_eq!(rt.retired(), 2);
}

/// HEX-GUEST-34 (decision 9: a host class with no record seeds from the slowest record of any class, every class's
/// records taken as one, then rises with the node's own): on Linux (KVM), whose boot-to-ready row isn't in
/// records.tsv yet, expect the first dial's bound that seed, the slowest boot_ready of any class with no margin,
/// never a refusal, and the recorded dial bound (AC-T7.4.1's managed patience) the same; then tell it of a boot slower
/// than the seed: expect the bound that boot, risen.
#[cfg(target_os = "linux")]
#[test]
fn an_unrecorded_class_seeds_from_the_slowest_of_any_class_and_rises() {
    let slowest_any = hex_client::guest::shards::RECORDS
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let row: Vec<&str> = l.split('\t').collect();
            (row.first() == Some(&"boot_ready")).then(|| row[2].parse::<u64>().unwrap())
        })
        .max()
        .unwrap();
    let seed = Duration::from_micros(slowest_any);
    let rt = GuestRuntime::new(Arc::new(Planned::new(&[Boots::Ready])), config());
    assert_eq!(recorded_boot().unwrap(), seed);
    assert_eq!(rt.first_dial_bound().unwrap(), seed);
    assert_eq!(recorded_dial_bound().unwrap(), seed);
    let slow = 3 * seed;
    rt.learn(slow);
    assert_eq!(rt.first_dial_bound().unwrap(), slow);
}

/// HEX-GUEST-34 (the lead: the record is the input that turns a class on; decision 9's seed for a class with no
/// record): read a records table with HVF's and WHP's rows: expect HVF its own row, and KVM, with none, the slowest of
/// any class (WHP's), with no margin; a measure no class records has no seed. Add a KVM row: expect KVM its own, with no code
/// between them.
#[test]
fn a_records_row_is_what_turns_a_class_on() {
    let hvf = "# measure\tclass\tslowest_us\trecord\nboot_ready\tmacOS-HVF\t265000\tr.md\n";
    assert_eq!(
        slowest_in(hvf, "boot_ready", "macOS-HVF"),
        Some(Duration::from_millis(265))
    );
    assert_eq!(slowest_in(hvf, "boot_ready", "Linux-KVM"), None);
    let two = format!("{hvf}boot_ready\tWindows-WHP\t2712939\tr.md\n");
    assert_eq!(
        seed_in(&two, "boot_ready", "macOS-HVF"),
        Some(Duration::from_millis(265))
    );
    assert_eq!(
        seed_in(&two, "boot_ready", "Linux-KVM"),
        Some(Duration::from_micros(2_712_939))
    );
    assert_eq!(seed_in(&two, "stop", "Linux-KVM"), None);
    let kvm = format!("{hvf}boot_ready\tLinux-KVM\t180000\tr.md\n");
    assert_eq!(
        slowest_in(&kvm, "boot_ready", "Linux-KVM"),
        Some(Duration::from_millis(180))
    );
    assert_eq!(slowest_in(&kvm, "stop", "Linux-KVM"), None);
    assert_eq!(
        seed_in(&kvm, "boot_ready", "Linux-KVM"),
        Some(Duration::from_millis(180))
    );
}

/// HEX-GUEST-39 (the lead, 2026-11-07: an upgrade's first boot costs 1.5-5.3 s of the host's first-run checks
/// before the guest's kernel runs, bench/native/guest/boot-ready/RECORD.md, and is no user's create's to pay):
/// make the node's runtime: expect its guest booted before it returns, and the guest a create asks for that one,
/// no second boot.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn the_node_boots_its_guest_before_it_accepts_a_box() {
    if shards_api::backend().is_err() {
        return;
    }
    let guests = Arc::new(Planned::new(&[Boots::Ready]));
    let node = WithGuest::new(
        hex_api::sim::SimRuntime::new(),
        GuestRuntime::new(guests.clone(), config()),
    )
    .unwrap();
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        1,
        "booted as it was made"
    );
    node.oci.guest().unwrap();
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        1,
        "a create boots none"
    );
}

/// HEX-GUEST-39: make a node whose guest can't boot: expect the node refused by type, naming the boot; and
/// one where Shards has no backend made, its `oci` class refused at create by name as before (§5), its
/// `process` class serving.
#[test]
fn a_node_whose_guest_cant_boot_is_refused_and_one_with_no_backend_is_made() {
    let failing = Arc::new(NoGuest {
        backend: Ok(shards_api::Backend::Kvm),
        boots: AtomicUsize::new(0),
    });
    let refused = WithGuest::new(
        hex_api::sim::SimRuntime::new(),
        GuestRuntime::new(failing.clone(), config()),
    )
    .err()
    .expect("a node whose guest can't boot is refused");
    assert!(
        refused.to_string().contains("booting the node's guest"),
        "{refused}"
    );
    assert_eq!(failing.boots.load(Ordering::SeqCst), 1);
    let none = Arc::new(NoGuest {
        backend: Err(shards_api::Refusal::NoBackend {
            os: "macos",
            arch: "x86_64",
        }),
        boots: AtomicUsize::new(0),
    });
    let node = WithGuest::new(
        hex_api::sim::SimRuntime::new(),
        GuestRuntime::new(none.clone(), config()),
    )
    .unwrap();
    assert_eq!(none.boots.load(Ordering::SeqCst), 0);
    assert_eq!(node.classes(), [Class::Process, Class::Oci]);
}

/// HEX-GUEST-40 (the lead, 2026-11-07): replace the Shards binary while the guest runs, then create twice: expect
/// one throwaway guest booted in the background on the new binary at Shards' least memory and stopped, so the
/// node's next boot is warm, the running guest left serving its boxes, and nothing more for a binary already
/// warmed.
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn a_changed_binary_is_warmed_once_in_the_background_and_the_guest_kept() {
    if shards_api::backend().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("shards");
    std::fs::write(&binary, b"a build").unwrap();
    let mut given = config();
    given.binary = binary.clone();
    let guests = Arc::new(Planned::new(&[Boots::Ready, Boots::Ready]));
    let rt = GuestRuntime::new(guests.clone(), given);
    rt.boot().unwrap();
    rt.warm_if_changed().unwrap();
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        1,
        "an unchanged binary warms nothing"
    );
    std::fs::write(&binary, b"the next build, an upgrade").unwrap();
    rt.warm_if_changed().unwrap();
    rt.warm_if_changed().unwrap();
    rt.finish_warm();
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        2,
        "one throwaway, however many creates"
    );
    assert_eq!(
        guests.stops.load(Ordering::SeqCst),
        1,
        "the throwaway stopped"
    );
    assert_eq!(
        guests.memory.lock().unwrap()[1],
        shards_api::min_memory_mib(std::env::consts::ARCH),
        "the throwaway at Shards' least memory"
    );
    rt.warm_if_changed().unwrap();
    rt.guest().unwrap();
    assert_eq!(
        guests.boots.load(Ordering::SeqCst),
        2,
        "the warmed binary and the running guest kept"
    );
}

/// Guests whose VMs outlive a lost handle, as Shards' do on macOS: each booted guest runs until stopped,
/// named by its identity, and `attach` of one still running gives it back. Counts boots, attaches and
/// stops; `answers` says whether a guest's relay answers a dial; `ended` how `attach` answers a guest that
/// no longer runs: `Gone`, as a simulator may, or what Shards' own take-back read of its process.
struct Kept {
    running: Mutex<std::collections::BTreeSet<u32>>,
    boots: AtomicUsize,
    attaches: AtomicUsize,
    stops: Arc<AtomicUsize>,
    answers: bool,
    ended: Mutex<Option<shards_api::ProcessFailure>>,
}

impl Kept {
    fn new(answers: bool) -> Arc<Self> {
        Arc::new(Self {
            running: Mutex::new(std::collections::BTreeSet::new()),
            boots: AtomicUsize::new(0),
            attaches: AtomicUsize::new(0),
            stops: Arc::new(AtomicUsize::new(0)),
            answers,
            ended: Mutex::new(None),
        })
    }

    fn guest(self: &Arc<Self>, pid: u32) -> Box<dyn shards_api::Guest> {
        Box::new(KeptGuest {
            pid,
            kept: Arc::clone(self),
        })
    }
}

fn kept_identity(pid: u32) -> shards_api::Identity {
    shards_api::Identity {
        pid,
        started: u64::from(pid) * 10,
        vsock: PathBuf::from(format!("/kept/{pid}.sock")),
    }
}

struct KeptGuest {
    pid: u32,
    kept: Arc<Kept>,
}

impl shards_api::Guest for KeptGuest {
    fn process_exited(&self) -> Result<bool, shards_api::Refusal> {
        Ok(!self.kept.running.lock().unwrap().contains(&self.pid))
    }

    fn connect(
        &self,
        port: u32,
        _patience: Duration,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        if self.kept.answers {
            Ok(Box::new(Looped(ready_relay())))
        } else {
            Err(shards_api::Refusal::Channel {
                port,
                why: "a stand-in that won't answer".to_owned(),
            })
        }
    }

    fn connect_before(
        &self,
        port: u32,
        at: std::time::Instant,
    ) -> Result<Box<dyn shards_api::Stream>, shards_api::Refusal> {
        let left = at
            .checked_duration_since(std::time::Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::Dial,
                },
            })?;
        let stream = self.connect(port, left)?;
        if std::time::Instant::now() >= at {
            return Err(shards_api::Refusal::Connect {
                port,
                failure: shards_api::HandshakeError::Expired {
                    phase: shards_api::HandshakePhase::ClearTimeouts,
                },
            });
        }
        Ok(stream)
    }

    fn stop(&self) -> Result<(), shards_api::Refusal> {
        self.kept.stops.fetch_add(1, Ordering::SeqCst);
        self.kept.running.lock().unwrap().remove(&self.pid);
        Ok(())
    }

    fn wait_for(&self, _: Duration) -> Result<Option<shards_api::End>, shards_api::Refusal> {
        let running = self.kept.running.lock().unwrap().contains(&self.pid);
        Ok(if running {
            None
        } else {
            Some(shards_api::End::Stopped)
        })
    }

    fn identity(&self) -> Result<shards_api::Identity, shards_api::Refusal> {
        Ok(kept_identity(self.pid))
    }
}

/// Wraps an Arc'd `Kept` as the runtime's `Guests`.
struct KeptGuests(Arc<Kept>);

impl shards_api::Guests for KeptGuests {
    fn probe_gpu(
        &self,
        _: &shards_api::GuestSpec,
        _: Duration,
    ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
        Err(shards_api::Refusal::GpuProbe(
            shards_api::GpuProbeError::Unavailable,
        ))
    }

    fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
        shards_api::backend()
    }

    fn boot(
        &self,
        _spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        let n = self.0.boots.fetch_add(1, Ordering::SeqCst);
        let pid = 100 + u32::try_from(n).unwrap();
        self.0.running.lock().unwrap().insert(pid);
        Ok(self.0.guest(pid))
    }

    fn attach(
        &self,
        identity: &shards_api::Identity,
        _spec: &shards_api::GuestSpec,
    ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
        self.0.attaches.fetch_add(1, Ordering::SeqCst);
        let running = self.0.running.lock().unwrap().contains(&identity.pid);
        if running && *identity == kept_identity(identity.pid) {
            Ok(self.0.guest(identity.pid))
        } else if let Some(failure) = self.0.ended.lock().unwrap().clone() {
            Err(shards_api::Refusal::Process {
                pid: identity.pid,
                expected_started: identity.started,
                failure,
            })
        } else {
            Err(shards_api::Refusal::Gone {
                pid: identity.pid,
                why: "no stand-in guest runs as it".to_owned(),
            })
        }
    }
}

/// A node on `kept`'s guests recording its guest at `record`.
fn kept_node(kept: &Arc<Kept>, record: &Path) -> GuestRuntime {
    GuestRuntime::new(Arc::new(KeptGuests(Arc::clone(kept))), config())
        .recorded_at(record.to_owned())
}

/// Native plan step 8 (T-T7.1.2): a node boots its guest and records it; the node is lost without its drop
/// (a crash: nothing of it stops the guest); the next node on the same record takes that guest back and
/// boots none, stops none.
#[test]
fn a_node_takes_back_the_guest_it_recorded_and_boots_none() {
    if shards_api::backend().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("@guest");
    let kept = Kept::new(true);
    let first = kept_node(&kept, &record);
    first.boot().unwrap();
    assert_eq!(held::read(&record).unwrap(), Some(kept_identity(100)));
    // Lost without its drop, as a crash loses it.
    let _lost = std::mem::ManuallyDrop::new(first);
    let next = kept_node(&kept, &record);
    next.boot().unwrap();
    assert_eq!(
        kept.boots.load(Ordering::SeqCst),
        1,
        "a second guest booted"
    );
    assert_eq!(kept.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(kept.stops.load(Ordering::SeqCst), 0);
    assert_eq!(held::read(&record).unwrap(), Some(kept_identity(100)));
}

/// Native plan step 8 (T-T7.1.2): a node whose recorded guest is gone forgets it and boots its own, which it
/// records in its place.
#[test]
fn a_recorded_guest_that_is_gone_is_forgotten_and_another_booted() {
    if shards_api::backend().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("@guest");
    held::write(&record, &kept_identity(7)).unwrap();
    let kept = Kept::new(true);
    let node = kept_node(&kept, &record);
    node.boot().unwrap();
    assert_eq!(kept.attaches.load(Ordering::SeqCst), 1);
    assert_eq!(kept.boots.load(Ordering::SeqCst), 1);
    assert_eq!(held::read(&record).unwrap(), Some(kept_identity(100)));
}

/// Native plan step 8 (T-T7.1.2), AC-T7.4.1: Shards' take-back refuses a guest whose process ended with what it
/// read of that process (D215: on every host, Windows' since Shards b4548fcf, where it answered `Gone`): seen to
/// have exited, or its pid another start's. Each proves the recorded guest ended. Do: start a node whose recorded
/// guest Shards answers so, then one whose guest's process Shards could not read. Expect: the first forgets it
/// and boots its own, recorded in its place; the second, which proves nothing, is refused and boots none, its
/// record kept.
#[test]
fn a_recorded_guest_shards_saw_end_is_forgotten_and_another_booted() {
    if shards_api::backend().is_err() {
        return;
    }
    for failure in [
        shards_api::ProcessFailure::Exited,
        shards_api::ProcessFailure::DifferentStart { observed: 71 },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("@guest");
        held::write(&record, &kept_identity(7)).unwrap();
        let kept = Kept::new(true);
        *kept.ended.lock().unwrap() = Some(failure.clone());
        let node = kept_node(&kept, &record);
        node.boot()
            .unwrap_or_else(|why| panic!("{failure:?}: {why}"));
        assert_eq!(kept.attaches.load(Ordering::SeqCst), 1);
        assert_eq!(kept.boots.load(Ordering::SeqCst), 1, "{failure:?}");
        assert_eq!(held::read(&record).unwrap(), Some(kept_identity(100)));
    }
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("@guest");
    held::write(&record, &kept_identity(7)).unwrap();
    let kept = Kept::new(true);
    *kept.ended.lock().unwrap() = Some(shards_api::ProcessFailure::Os {
        operation: shards_api::ProcessOperation::OpenProcess,
        code: 5,
    });
    let node = kept_node(&kept, &record);
    assert!(node.boot().is_err(), "an unread guest taken as ended");
    assert_eq!(
        kept.boots.load(Ordering::SeqCst),
        0,
        "a guest booted beside one that may run"
    );
    assert_eq!(held::read(&record).unwrap(), Some(kept_identity(7)));
}

/// Native plan step 8 (no orphan VM): a guest taken back that won't answer is stopped before the node boots
/// another, so none runs beside the next.
#[test]
fn a_taken_back_guest_that_wont_answer_is_stopped_before_another_boots() {
    if shards_api::backend().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("@guest");
    let kept = Kept::new(false);
    kept.running.lock().unwrap().insert(7);
    held::write(&record, &kept_identity(7)).unwrap();
    let node = kept_node(&kept, &record);
    // Its own boot won't answer either: refused, as a boot that never readies is.
    let _ = node.boot();
    assert_eq!(kept.attaches.load(Ordering::SeqCst), 1);
    assert!(
        !kept.running.lock().unwrap().contains(&7),
        "the guest taken back runs on: an orphan"
    );
    assert!(kept.stops.load(Ordering::SeqCst) >= 1);
}

/// Native plan step 8: a node that stops its guest (dropped) removes its record, so the next node takes
/// back nothing.
#[test]
fn a_node_that_stops_its_guest_leaves_no_record() {
    if shards_api::backend().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("@guest");
    let kept = Kept::new(true);
    let node = kept_node(&kept, &record);
    node.boot().unwrap();
    assert!(held::read(&record).unwrap().is_some());
    drop(node);
    assert_eq!(held::read(&record).unwrap(), None);
    assert!(kept.running.lock().unwrap().is_empty());
}

struct CapacityLink(Option<hex_api::Capacity>);
impl Link for CapacityLink {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        assert_eq!(port, stream::CONTROL_PORT);
        hex_client::guest::own_duplex(ready_relay_with_answer(Some(
            hex_core::wire::Answer::Capacity(
                self.0
                    .as_ref()
                    .map(hex_api::Capacity::try_clone)
                    .transpose()?,
            ),
        )))
    }
}

fn capacity_runtime(capacity: Option<hex_api::Capacity>, config: GuestConfig) -> GuestRuntime {
    let kept = Kept::new(true);
    kept.running.lock().unwrap().insert(123);
    let guest: Arc<dyn shards_api::Guest> = kept.guest(123).into();
    let link: LinkOwner = own_link(CapacityLink(capacity)).unwrap();
    let client = Client::connect(triomphe::Arc::clone(&link)).unwrap();
    let runtime = GuestRuntime::new(Arc::new(KeptGuests(kept)), config);
    *runtime.booted.lock().unwrap() = Some(
        booted_owner(Booted {
            guest: GuestHandle::Shared(guest),
            link,
            client,
            evented: Mutex::new(None),
            owner: None,
            backing: None,
        })
        .unwrap(),
    );
    runtime
}

pub(super) fn model_connection() -> (LinkOwner, Client) {
    let link: LinkOwner = own_link(CapacityLink(None)).unwrap();
    let client = Client::connect(triomphe::Arc::clone(&link)).unwrap();
    (link, client)
}

/// AC-T7.4.1/r329: the actual guest-node reply, not host RAM, supplies the pool; a 633MiB
/// request cannot gain authority from a guest configured with only632MiB.
#[test]
fn runtime_capacity_forwards_guest_room_clamps_config_and_preserves_guest_disk() {
    let mut config = config();
    config.vcpus = 2;
    config.memory_mib = 632;
    let reported = hex_api::Capacity {
        devices: Vec::new(),
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        id: "inner-node".into(),
        total: hex_api::CapacityResources {
            cpu_milli: 4_000,
            mem_bytes: 1024 << 20,
            disk_bytes: 8 << 30,
        },
        free: hex_api::CapacityResources {
            cpu_milli: 3_000,
            mem_bytes: 900 << 20,
            disk_bytes: 7 << 30,
        },
        unavailable: Vec::new(),
        learned: None,
    };
    let runtime = capacity_runtime(Some(reported.try_clone().unwrap()), config);
    let actual = runtime.capacity(Class::Oci).unwrap().unwrap();
    assert_eq!(actual.id, "guest:123:1230");
    assert_eq!(
        (actual.total.cpu_milli, actual.free.cpu_milli),
        (2_000, 2_000)
    );
    assert_eq!(
        (actual.total.mem_bytes, actual.free.mem_bytes),
        (632 << 20, 632 << 20)
    );
    assert_eq!(
        (actual.total.disk_bytes, actual.free.disk_bytes),
        (reported.total.disk_bytes, reported.free.disk_bytes)
    );
    assert!(runtime.capacity(Class::Process).is_err());
}

/// AC-T7.4.1: no guest capacity is a refusal; native Process still explicitly uses host facts.
#[test]
fn runtime_capacity_guest_missing_is_refused_native_default_is_host_fallback() {
    let guest = capacity_runtime(None, config());
    let combined = WithGuest::new(hex_api::sim::SimRuntime::new(), guest).unwrap();
    assert_eq!(combined.capacity(Class::Process).unwrap(), None);
    assert!(matches!(
        combined.capacity(Class::Oci),
        Err(Refusal::Broken(_))
    ));
}

/// AC-T7.4.1: a configured ceiling never inflates a smaller actual guest or its free room.
#[test]
fn runtime_capacity_keeps_smaller_actual_guest_room() {
    let mut configured = config();
    configured.vcpus = 4;
    configured.memory_mib = 632;
    let room = hex_api::CapacityResources {
        cpu_milli: 500,
        mem_bytes: 500 << 20,
        disk_bytes: 1 << 30,
    };
    let free = hex_api::CapacityResources {
        cpu_milli: 250,
        mem_bytes: 400 << 20,
        disk_bytes: 512 << 20,
    };
    let actual = guest_capacity(
        hex_api::Capacity {
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            id: "inner".into(),
            total: room,
            free,
            unavailable: Vec::new(),
            learned: None,
        },
        &configured,
        &kept_identity(123),
    )
    .unwrap();
    assert_eq!((actual.total, actual.free), (room, free));
}

#[path = "device_routing_tests.rs"]
mod device_routing_tests;

/// Reuse the actual native-prefix adapter in the same-owner composition controls.
pub(super) fn native_reporting(report: hex_api::Capacity) -> impl Runtime {
    device_routing_tests::NativeReporting {
        inner: hex_api::sim::SimRuntime::new(),
        report,
    }
}
