//! What a staged folder's file system has free, as the relay learns it (docs/design/shards-guest.md §4a, the
//! learned budget): from its inode counts where it keeps them (tmpfs, ext4), and where it keeps none (btrfs:
//! statfs(2), "fields that are undefined for a particular filesystem are set to 0") from its bytes, since every
//! entry takes at least one byte of the file system.

use super::*;

fn stat(files: u64, ffree: u64, blocks: u64, bfree: u64, bavail: u64) -> Stat {
    Stat {
        files,
        ffree,
        blocks,
        bfree,
        bavail,
        frsize: 4096,
    }
}

/// HEX-GUEST-14 (§4a; CLAUDE.md §2 and §8): a file system that keeps inode counts (tmpfs here: 1,048,576
/// inodes, 151 in use): expect its free inodes the entries a stage may make, its used inodes the most a
/// removal visits, and its inodes the most layers its store can hold. One that keeps none (btrfs: f_files and f_ffree 0, as fedora's /home reported, where every
/// relay stage was refused "entries in one stage, bound 0"): expect its free bytes the entries a stage may
/// make, its used bytes the most a removal visits, never 0, and its bytes the most layers it can hold.
#[test]
fn a_file_system_without_inode_counts_bounds_entries_by_its_bytes() {
    let tmpfs = learned(&stat(1_048_576, 1_048_425, 1000, 900, 900));
    assert_eq!(tmpfs.entries, 1_048_425);
    assert_eq!(tmpfs.used, 151);
    assert_eq!(tmpfs.bytes, 900 * 4096);
    assert_eq!(tmpfs.capacity, 1_048_576);
    let btrfs = learned(&stat(0, 0, 1000, 300, 250));
    assert_eq!(btrfs.entries, 250 * 4096);
    assert_eq!(btrfs.used, 700 * 4096);
    assert_eq!(btrfs.bytes, 250 * 4096);
    assert_eq!(btrfs.capacity, 1000 * 4096);
}
