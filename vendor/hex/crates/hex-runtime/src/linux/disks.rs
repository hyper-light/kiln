//! The disks a path's files live on: what a box's `io.latency` and `io.max` apply to. The kernel
//! takes those per whole disk, never per partition (blkcg_conf_open_bdev refuses partitions), so a
//! partition is followed to its disk, and a device-mapper device (LUKS, LVM) to the disks below it.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use hex_api::Refusal;
use hex_core::cgroup::Dev;

use super::os_err;

/// Deepest stack of device-mapper devices followed (LUKS on LVM on RAID is three).
const MAX_DEPTH: usize = 8;
/// Most disks one filesystem is followed to.
const MAX_DISKS: usize = 64;

/// The whole disks under `path`'s filesystem.
pub fn under(path: &Path) -> Result<Vec<Dev>, Refusal> {
    let meta = fs::metadata(path).map_err(|e| os_err("finding a path's disks", path, e))?;
    let dev = split(meta.dev());
    let start = if dev.major == 0 {
        // btrfs and other filesystems over several devices report an anonymous device; the mount's
        // source names the real one. A filesystem with no block device at all (tmpfs, overlay)
        // can't be rate-limited by disk, so it resolves to no disks: `io.latency` and `io.max`
        // then have nothing to apply to, which is correct, while `io.weight` still applies.
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|e| Refusal::os("reading /proc/self/mountinfo", e))?;
        let Some(source) = mount_source(&mountinfo, dev) else {
            return Ok(vec![]);
        };
        match fs::metadata(&source) {
            Ok(m) if m.rdev() != 0 => split(m.rdev()),
            _ => return Ok(vec![]),
        }
    } else {
        dev
    };
    let mut disks = BTreeSet::new();
    let mut todo = vec![(start, 0usize)];
    while let Some((d, depth)) = todo.pop() {
        if disks.len() >= MAX_DISKS || depth > MAX_DEPTH {
            break;
        }
        let sys = PathBuf::from(format!("/sys/dev/block/{}:{}", d.major, d.minor));
        let slaves: Vec<Dev> = fs::read_dir(sys.join("slaves"))
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter_map(|e| fs::read_to_string(e.path().join("dev")).ok())
                    .filter_map(|t| parse_dev(&t))
                    .take(MAX_DISKS)
                    .collect()
            })
            .unwrap_or_default();
        if !slaves.is_empty() {
            todo.extend(slaves.into_iter().map(|s| (s, depth.saturating_add(1))));
        } else if sys.join("partition").exists() {
            let disk = fs::read_to_string(sys.join("../dev"))
                .ok()
                .and_then(|t| parse_dev(&t))
                .ok_or_else(|| Refusal::Os {
                    op: "finding a partition's disk",
                    why: format!("{}: no parent dev", sys.display()),
                })?;
            disks.insert(disk);
        } else {
            disks.insert(d);
        }
    }
    Ok(disks.into_iter().collect())
}

fn split(dev: u64) -> Dev {
    Dev {
        major: rustix::fs::major(dev),
        minor: rustix::fs::minor(dev),
    }
}

/// "259:3\n" as a device.
pub fn parse_dev(text: &str) -> Option<Dev> {
    let (major, minor) = text.trim().split_once(':')?;
    Some(Dev {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    })
}

/// The source of the mount of device `dev`, from `/proc/self/mountinfo`'s text (proc(5): field 3
/// is major:minor, and the source follows the " - " separator and the filesystem type).
pub fn mount_source(mountinfo: &str, dev: Dev) -> Option<String> {
    let want = format!("{}:{}", dev.major, dev.minor);
    mountinfo.lines().find_map(|line| {
        let mut fields = line.split(' ');
        if fields.nth(2)? != want {
            return None;
        }
        let (_, after) = line.split_once(" - ")?;
        let mut post = after.split(' ');
        let _fstype = post.next()?;
        post.next().map(unescape)
    })
}

/// mountinfo escapes space, tab, newline and backslash as octal (`\040`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = chars.clone().take(3).collect();
            if digits.len() == 3
                && let Ok(n) = u8::from_str_radix(&digits, 8)
            {
                out.push(char::from(n));
                for _ in 0..3 {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:23 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
29 1 0:34 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p3 rw,seclabel,compress=zstd:1
31 29 0:34 /home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p3 rw,seclabel
40 29 259:5 / /data\\040disk rw,relatime shared:9 - ext4 /dev/my\\040disk rw
";

    #[test]
    fn a_mount_is_found_by_its_device_and_its_source_unescaped() {
        assert_eq!(
            mount_source(
                MOUNTINFO,
                Dev {
                    major: 0,
                    minor: 34
                }
            )
            .as_deref(),
            Some("/dev/nvme0n1p3")
        );
        assert_eq!(
            mount_source(
                MOUNTINFO,
                Dev {
                    major: 259,
                    minor: 5
                }
            )
            .as_deref(),
            Some("/dev/my disk")
        );
        assert_eq!(mount_source(MOUNTINFO, Dev { major: 9, minor: 9 }), None);
    }

    #[test]
    fn device_numbers_parse_and_garbage_doesnt() {
        assert_eq!(
            parse_dev("259:3\n"),
            Some(Dev {
                major: 259,
                minor: 3
            })
        );
        for bad in ["", "259", "a:b", "1:2:3", ":"] {
            assert_eq!(parse_dev(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hostile_mountinfo_lines_are_skipped_not_a_panic() {
        for line in [
            "",
            " - ",
            "1 2",
            "1 2 0:34",
            "1 2 0:34 / / rw - btrfs",
            "\\\\\\0",
            "1 2 0:34 x y - t \\04",
        ] {
            let _ = mount_source(
                line,
                Dev {
                    major: 0,
                    minor: 34,
                },
            );
        }
    }
}
