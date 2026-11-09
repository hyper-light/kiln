//! Linux: the work directory's file system and the block devices under it, found without
//! privilege (docs/needs.md; research-disk.md): `stat`'s `st_dev` → `/proc/self/mountinfo` (its
//! third field) → `/sys/dev/block/<maj:min>`, a partition to its disk, a device-mapper or md
//! device to its members (`slaves/`). btrfs's `st_dev` is anonymous: its devices are
//! `/sys/fs/btrfs/<fsid>/devices/`. An overlay is followed to its `upperdir`, where writes go.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use kiln_ci::facts::{DeviceKind, Storage, StorageDevice};

use crate::sense::disk::{Counters, Errors, linux};

/// One `/proc/self/mountinfo` line's fields this needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mount {
    pub dev: (u32, u32),
    pub mount: String,
    pub fs: String,
    pub source: String,
    pub super_opts: String,
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\040`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            let code: String = it.clone().take(3).collect();
            if code.len() == 3
                && let Ok(n) = u8::from_str_radix(&code, 8)
            {
                out.push(char::from(n));
                for _ in 0..3 {
                    it.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// `/proc/self/mountinfo`'s lines: `id parent maj:min root mountpoint options [optional…] - fstype
/// source superoptions`.
pub fn mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let mut w = left.split_whitespace();
            let (_, _, dev, _, mount) = (w.next()?, w.next()?, w.next()?, w.next()?, w.next()?);
            let (maj, min) = dev.split_once(':')?;
            let mut r = right.split_whitespace();
            Some(Mount { dev: (maj.parse().ok()?, min.parse().ok()?), mount: unescape(mount), fs: r.next()?.to_owned(), source: unescape(r.next().unwrap_or_default()), super_opts: r.next().unwrap_or_default().to_owned() })
        })
        .collect()
}

/// The mount `path` (canonical) is on: of those whose device is `dev`, the one with the longest
/// mount point above it, the last of equals (a later mount covers an earlier); without a match by
/// device, by mount point alone.
pub fn mount_of<'a>(mounts: &'a [Mount], path: &Path, dev: (u32, u32)) -> Option<&'a Mount> {
    let above = |m: &&Mount| path.starts_with(&m.mount);
    let best = |it: Vec<&'a Mount>| it.into_iter().rev().max_by_key(|m| m.mount.len());
    best(mounts.iter().filter(above).filter(|m| m.dev == dev).collect()).or_else(|| best(mounts.iter().filter(above).collect()))
}

/// File systems served over the network (`statfs` would say NFS 0x6969, SMB2 0xFE534D42, Ceph
/// 0x00c36400; mountinfo names them).
pub fn network_fs(fs: &str, source: &str) -> bool {
    matches!(fs, "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "ceph" | "glusterfs" | "lustre" | "afs" | "9p" | "gpfs" | "beegfs" | "ocfs2" | "gfs2") || (fs.starts_with("fuse.") && (source.contains(':') || source.contains('@')))
}

/// A whole disk's kind and interconnect, from what sysfs says of it: its name, its device path
/// under `/sys/devices` (the bus it hangs off), `queue/rotational`, an NVMe controller's
/// `transport`, and its vendor and model.
pub fn classify(name: &str, devpath: &str, rotational: Option<bool>, nvme_transport: Option<&str>, vendor_model: &str) -> (DeviceKind, Option<String>) {
    let vm = vendor_model.to_ascii_lowercase();
    let virtual_model = ["qemu", "vmware", "virtual disk", "virtual hd", "vbox", "msft virtual"].iter().any(|v| vm.contains(v));
    if name.starts_with("zram") || name.starts_with("ram") || name.starts_with("pmem") {
        return (DeviceKind::Ram, None);
    }
    if name.starts_with("loop") {
        return (DeviceKind::Virtual, Some("loop".into()));
    }
    if name.starts_with("nbd") || name.starts_with("rbd") || name.starts_with("drbd") {
        return (DeviceKind::Network, Some(name.trim_end_matches(|c: char| c.is_ascii_digit()).to_owned()));
    }
    if name.starts_with("vd") || devpath.contains("/virtio") {
        return (DeviceKind::Virtual, Some("virtio".into()));
    }
    if name.starts_with("xvd") || devpath.contains("/vbd-") {
        return (DeviceKind::Virtual, Some("xen".into()));
    }
    if name.starts_with("nvme") {
        let t = nvme_transport.unwrap_or("pcie");
        let kind = match t {
            "pcie" if virtual_model => DeviceKind::Virtual,
            "pcie" => DeviceKind::Nvme,
            "loop" => DeviceKind::Virtual,
            _ => DeviceKind::Network,
        };
        return (kind, Some(t.to_owned()));
    }
    if devpath.contains("/session") {
        return (DeviceKind::Network, Some("iscsi".into()));
    }
    let bus = if devpath.contains("/usb") {
        Some("usb")
    } else if devpath.contains("/ata") {
        Some("sata")
    } else if name.starts_with("mmcblk") {
        Some("mmc")
    } else if devpath.contains("/host") && devpath.contains("/target") {
        Some("scsi")
    } else {
        None
    };
    if virtual_model {
        return (DeviceKind::Virtual, bus.map(str::to_owned));
    }
    let kind = match (rotational, name.starts_with("mmcblk")) {
        (_, true) | (Some(false), _) => DeviceKind::Ssd,
        (Some(true), _) => DeviceKind::Hdd,
        (None, _) => DeviceKind::Unknown,
    };
    (kind, bus.map(str::to_owned))
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn file_name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// A block device's sysfs directory (`/sys/devices/…/nvme0n1/nvme0n1p3`) to its whole disk's.
fn whole(dir: PathBuf) -> PathBuf {
    if dir.join("partition").exists()
        && let Some(parent) = dir.parent()
    {
        return parent.to_path_buf();
    }
    dir
}

/// The whole disks under a block device: itself, or its members (`slaves/`), followed down.
fn leaves(disk: &Path, seen: &mut BTreeSet<PathBuf>, out: &mut Vec<PathBuf>) {
    if !seen.insert(disk.to_path_buf()) {
        return;
    }
    let members: Vec<PathBuf> = std::fs::read_dir(disk.join("slaves")).map(|d| d.flatten().filter_map(|e| e.path().canonicalize().ok()).map(whole).collect()).unwrap_or_default();
    if members.is_empty() {
        out.push(disk.to_path_buf());
    }
    for m in members {
        leaves(&m, seen, out);
    }
}

fn block_dir(dev: (u32, u32)) -> Option<PathBuf> {
    Path::new(&format!("/sys/dev/block/{}:{}", dev.0, dev.1)).canonicalize().ok().map(whole)
}

/// A `/dev/…` source's whole disk directory.
fn block_of_source(source: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt as _;
    let meta = std::fs::metadata(source).ok()?;
    if !meta.file_type().is_block_device() {
        return None;
    }
    let rdev = meta.rdev();
    block_dir((nix::sys::stat::major(rdev).try_into().ok()?, nix::sys::stat::minor(rdev).try_into().ok()?))
}

/// What the work directory is on.
struct Found {
    mount: Mount,
    /// The disks the file system sits on directly (a dm or md device, a disk; several for btrfs).
    tops: Vec<PathBuf>,
    /// btrfs's `/sys/fs/btrfs/<fsid>`.
    btrfs: Option<PathBuf>,
}

fn find(path: &Path) -> Option<Found> {
    let mounts = mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").ok()?);
    let mut path = path.to_path_buf();
    let mut seen = BTreeSet::new();
    loop {
        let st = std::fs::metadata(&path).ok()?;
        let dev = st.dev();
        let dev = (u32::try_from(nix::sys::stat::major(dev)).ok()?, u32::try_from(nix::sys::stat::minor(dev)).ok()?);
        let m = mount_of(&mounts, &path, dev)?.clone();
        // An overlay writes to its upper directory: follow it, once each.
        if m.fs == "overlay"
            && let Some(upper) = m.super_opts.split(',').find_map(|o| o.strip_prefix("upperdir="))
            && seen.insert(upper.to_owned())
        {
            path = PathBuf::from(unescape(upper));
            continue;
        }
        if m.fs == "btrfs" {
            let src = Path::new(&m.source).canonicalize().ok().and_then(|p| file_name(&p));
            let fsid = std::fs::read_dir("/sys/fs/btrfs").ok()?.flatten().map(|e| e.path()).find(|d| src.as_ref().is_some_and(|s| d.join("devices").join(s).exists()));
            let tops = fsid.as_ref().and_then(|d| std::fs::read_dir(d.join("devices")).ok()).map(|it| it.flatten().filter_map(|e| e.path().canonicalize().ok()).map(whole).collect()).unwrap_or_default();
            return Some(Found { mount: m, tops, btrfs: fsid });
        }
        let top = if m.dev.0 != 0 { block_dir(m.dev) } else { None }.or_else(|| block_of_source(Path::new(&m.source)));
        return Some(Found { mount: m, tops: top.into_iter().collect(), btrfs: None });
    }
}

/// An error counter file and what it counts.
#[derive(Debug, Clone, Copy)]
enum Count {
    Errors,
    Timeouts,
    Retries,
    Resets,
    /// btrfs's `error_stats`: a sum of its lines.
    Btrfs,
}

/// The work volume's devices, as found once: where their counters are.
pub struct Device {
    stats: Vec<PathBuf>,
    errors: Vec<(PathBuf, Count)>,
}

impl Device {
    pub fn of(work_dir: &Path) -> Option<Device> {
        let f = find(work_dir)?;
        if f.tops.is_empty() {
            return None;
        }
        let stats: Vec<PathBuf> = f.tops.iter().map(|t| t.join("stat")).filter(|p| p.exists()).collect();
        let mut errors = Vec::new();
        let mut leaves_all = Vec::new();
        let mut seen = BTreeSet::new();
        for t in &f.tops {
            leaves(t, &mut seen, &mut leaves_all);
            // md's members' errors (`md/dev-*/errors`).
            if let Ok(d) = std::fs::read_dir(t.join("md")) {
                errors.extend(d.flatten().map(|e| e.path().join("errors")).filter(|p| p.exists()).map(|p| (p, Count::Errors)));
            }
        }
        for leaf in &leaves_all {
            let dev = leaf.join("device");
            for (file, what) in [(dev.join("ioerr_cnt"), Count::Errors), (dev.join("iotmo_cnt"), Count::Timeouts), (leaf.join("diag/command_error_count"), Count::Errors), (leaf.join("diag/command_retries_count"), Count::Retries), (dev.join("diag/adm_errors"), Count::Errors), (dev.join("diag/reset_count"), Count::Resets), (dev.join("diag/reconnect_count"), Count::Resets)] {
                if file.exists() {
                    errors.push((file, what));
                }
            }
        }
        if let Some(fsid) = &f.btrfs
            && let Ok(d) = std::fs::read_dir(fsid.join("devinfo"))
        {
            errors.extend(d.flatten().map(|e| e.path().join("error_stats")).filter(|p| p.exists()).map(|p| (p, Count::Btrfs)));
        }
        // ext4's own count of errors it met (`/sys/fs/ext4/<device>/errors_count`).
        if f.mount.fs == "ext4"
            && let Some(name) = Path::new(&f.mount.source).canonicalize().ok().and_then(|p| file_name(&p))
        {
            let p = Path::new("/sys/fs/ext4").join(name).join("errors_count");
            if p.exists() {
                errors.push((p, Count::Errors));
            }
        }
        (!stats.is_empty()).then_some(Device { stats, errors })
    }

    /// The devices' counters now, summed over them (a btrfs over several): one `stat` line each,
    /// and the error counters found.
    pub fn counters(&mut self) -> Option<Counters> {
        let mut total: Option<Counters> = None;
        for p in &self.stats {
            let mut buf = [0u8; 256];
            let Some(c) = read_small(p, &mut buf).and_then(linux::stat) else { continue };
            total = Some(match total {
                None => c,
                Some(t) => Counters {
                    read: crate::sense::disk::Dir { ops: t.read.ops + c.read.ops, bytes: t.read.bytes + c.read.bytes, time_ns: t.read.time_ns + c.read.time_ns },
                    write: crate::sense::disk::Dir { ops: t.write.ops + c.write.ops, bytes: t.write.bytes + c.write.bytes, time_ns: t.write.time_ns + c.write.time_ns },
                    // Several devices' busy times don't add up to one's.
                    busy_ns: None,
                    in_flight_ns: t.in_flight_ns.zip(c.in_flight_ns).map(|(a, b)| a + b),
                    errors: Errors::default(),
                },
            });
        }
        let mut total = total?;
        let mut e = Errors::default();
        for (p, what) in &self.errors {
            let mut buf = [0u8; 256];
            let Some(text) = read_small(p, &mut buf) else { continue };
            let n = match what {
                Count::Btrfs => linux::btrfs_errors(text),
                _ => linux::counter(text),
            };
            let add = Errors {
                errors: n.filter(|_| matches!(what, Count::Errors | Count::Btrfs)),
                timeouts: n.filter(|_| matches!(what, Count::Timeouts)),
                retries: n.filter(|_| matches!(what, Count::Retries)),
                resets: n.filter(|_| matches!(what, Count::Resets)),
            };
            e = e.plus(add);
        }
        total.errors = e;
        Some(total)
    }
}

fn read_small<'a>(p: &Path, buf: &'a mut [u8]) -> Option<&'a str> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(p).ok()?;
    let n = f.read(buf).ok()?;
    std::str::from_utf8(buf.get(..n)?).ok()
}

/// One whole disk's facts.
fn device_facts(dir: &Path) -> StorageDevice {
    let name = file_name(dir).unwrap_or_default();
    let devpath = dir.to_string_lossy().into_owned();
    let rotational = read(&dir.join("queue/rotational")).map(|r| r == "1");
    let dev = dir.join("device");
    let transport = read(&dev.join("transport"));
    let vendor = read(&dev.join("vendor")).unwrap_or_default();
    let model = read(&dev.join("model"));
    let (kind, interconnect) = classify(&name, &devpath, rotational, transport.as_deref(), &format!("{vendor} {}", model.as_deref().unwrap_or_default()));
    let size = read(&dir.join("size")).and_then(|s| s.parse::<u64>().ok()).map(|s| s.saturating_mul(512));
    StorageDevice { name, kind, interconnect, rotational, model, size, location: None, health: None }
}

/// The work directory's storage (docs/needs.md): its file system, and each whole disk under it.
pub fn facts(work_dir: &Path) -> Option<Storage> {
    let work_dir = crate::sense::canonical(work_dir);
    let f = find(&work_dir)?;
    let network = network_fs(&f.mount.fs, &f.mount.source);
    let mut devices = Vec::new();
    if matches!(f.mount.fs.as_str(), "tmpfs" | "ramfs") {
        devices.push(StorageDevice { name: f.mount.fs.clone(), kind: DeviceKind::Ram, ..StorageDevice::default() });
    } else if network {
        devices.push(StorageDevice { name: f.mount.source.clone(), kind: DeviceKind::Network, interconnect: Some(f.mount.fs.clone()), ..StorageDevice::default() });
    } else {
        let mut seen = BTreeSet::new();
        let mut all = Vec::new();
        for t in &f.tops {
            leaves(t, &mut seen, &mut all);
        }
        devices = all.iter().map(|d| device_facts(d)).collect();
    }
    Some(Storage { fs: Some(f.mount.fs), mount: Some(f.mount.mount), source: Some(f.mount.source).filter(|s| !s.is_empty()), network, devices })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/needs.md: path → mountinfo by device → the mount above it; btrfs's anonymous device
    /// and a later mount over an earlier.
    #[test]
    fn w13_r01_a_path_finds_its_mount() {
        let text = "29 1 0:26 /root / rw,relatime shared:1 - btrfs /dev/nvme1n1p3 rw,compress=zstd:1\n\
                    60 29 0:26 /home /home rw,relatime shared:30 - btrfs /dev/nvme1n1p3 rw\n\
                    61 29 259:2 / /boot rw - ext4 /dev/nvme1n1p2 rw\n\
                    90 60 0:50 / /home/a/my\\040dir rw - nfs4 srv:/export rw\n\
                    95 29 0:60 / /var/lib/docker/overlay2/x/merged rw - overlay overlay rw,lowerdir=/l,upperdir=/var/lib/docker/overlay2/x/diff,workdir=/w\n";
        let m = mountinfo(text);
        assert_eq!(m.len(), 5);
        assert_eq!(mount_of(&m, Path::new("/home/a/work"), (0, 26)).map(|m| m.mount.as_str()), Some("/home"));
        assert_eq!(mount_of(&m, Path::new("/home/a/my dir/x"), (0, 50)).map(|m| (m.fs.as_str(), m.source.as_str())), Some(("nfs4", "srv:/export")));
        assert_eq!(mount_of(&m, Path::new("/boot/efi"), (259, 2)).map(|m| m.dev), Some((259, 2)));
        assert!(network_fs("nfs4", "srv:/export") && network_fs("fuse.sshfs", "me@host:/x") && !network_fs("fuse.gocryptfs", "/home/x/.enc") && !network_fs("ext4", "/dev/sda1"));
        let o = mount_of(&m, Path::new("/var/lib/docker/overlay2/x/merged/app"), (0, 60));
        assert_eq!(o.and_then(|o| o.super_opts.split(',').find_map(|s| s.strip_prefix("upperdir="))), Some("/var/lib/docker/overlay2/x/diff"));
    }

    /// The kind is the interconnect's word before the medium flag's: a virtio disk declares it
    /// rotates, an NVMe over TCP is the network's.
    #[test]
    fn w13_r01_device_kind_by_interconnect() {
        let pci = "/sys/devices/pci0000:00/0000:00:1d.0/0000:2d:00.0/nvme/nvme1/nvme1n1";
        assert_eq!(classify("nvme1n1", pci, Some(false), Some("pcie"), "Samsung SSD 990"), (DeviceKind::Nvme, Some("pcie".into())));
        assert_eq!(classify("nvme2n1", "/sys/devices/virtual/nvme-fabrics/ctl/nvme2/nvme2n1", Some(false), Some("tcp"), "Linux"), (DeviceKind::Network, Some("tcp".into())));
        assert_eq!(classify("vda", "/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vda", Some(true), None, ""), (DeviceKind::Virtual, Some("virtio".into())));
        assert_eq!(classify("sda", "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda", Some(true), None, "ATA WDC WD40"), (DeviceKind::Hdd, Some("sata".into())));
        assert_eq!(classify("sdb", "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/host6/target6:0:0/6:0:0:0/block/sdb", Some(false), None, "Samsung T7"), (DeviceKind::Ssd, Some("usb".into())));
        assert_eq!(classify("sdc", "/sys/devices/pci0000:00/host2/target2:0:0/2:0:0:0/block/sdc", Some(true), None, "QEMU QEMU HARDDISK"), (DeviceKind::Virtual, Some("scsi".into())));
        assert_eq!(classify("sdd", "/sys/devices/platform/host7/session1/target7:0:0/7:0:0:0/block/sdd", Some(true), None, "LIO-ORG"), (DeviceKind::Network, Some("iscsi".into())));
        assert_eq!(classify("zram0", "/sys/devices/virtual/block/zram0", Some(false), None, "").0, DeviceKind::Ram);
        assert_eq!(classify("loop3", "/sys/devices/virtual/block/loop3", Some(false), None, "").0, DeviceKind::Virtual);
    }

    /// This machine's work volume, where it has one: a file system, and devices with a kind.
    #[test]
    fn w13_r01_this_machines_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let s = facts(dir.path()).ok_or_else(|| anyhow::anyhow!("no storage for {}", dir.path().display()))?;
        assert!(s.fs.is_some() && s.mount.is_some(), "{s:?}");
        if let Some(mut d) = Device::of(dir.path()) {
            let c = d.counters();
            assert!(c.is_some(), "counters for {s:?}");
        }
        Ok(())
    }
}
