//! The exact `/dev` tmpfs configuration a node may prepare before a pruned create. Kernel work is
//! confined to the refill worker; eligibility and target guards keep ordinary mount ordering.
use super::{mount::Op, spec::Spec};
use hex_core::devfs::Unpooled;
use rustix::mount::MountFlags;

/// What a prepared `/dev` is made for: its mount's source, data and flags. Every byte of it is the spec's, so a
/// key is bounded by the spec's own bound (`spec::MAX_CONFIG_BYTES`), and the pool's four keys by four of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub source: String,
    pub data: String,
    pub flags: MountFlags,
}

/// One writable tmpfs `/dev`, with no custom devices. Other cases keep today's full setup.
pub fn eligible(spec: &Spec, plan: &[Op]) -> Result<(u32, Key), Unpooled> {
    if !spec.linux.devices.is_empty() {
        return Err(Unpooled::Devices);
    }
    let mut devs = plan
        .iter()
        .enumerate()
        .filter(|(_, op)| is_dev(&op.destination));
    let (index, op) = devs.next().ok_or(Unpooled::Missing)?;
    if devs.next().is_some() {
        return Err(Unpooled::Duplicate);
    }
    if op.is_bind || op.fstype != "tmpfs" {
        return Err(Unpooled::Kind);
    }
    if op.flags.contains(MountFlags::RDONLY) {
        return Err(Unpooled::Readonly);
    }
    // Early defaults can change which operation exhausts an explicit inode quota. Ordinary setup
    // preserves both the caller's limits and the operation/refusal that reaches them first.
    if op
        .data
        .split(',')
        .any(|s| s.split('=').next() == Some("nr_inodes"))
    {
        return Err(Unpooled::Inodes);
    }
    Ok((
        u32::try_from(index).map_err(|_| Unpooled::Capacity)?,
        Key {
            source: op.kind_source.clone(),
            data: op.data.clone(),
            flags: op.flags,
        },
    ))
}

fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty() && *s != ".")
}

pub(crate) fn is_dev(path: &str) -> bool {
    let mut p = parts(path);
    p.next() == Some("dev") && p.next().is_none()
}

/// A later mount must not use a default created early or replace `/dev` or its root. Symlink paths
/// are checked separately against the actual root immediately before each later mount.
pub(crate) fn target_unaffected(path: &str) -> bool {
    if parts(path).any(|part| part == "..") {
        return false;
    }
    let mut p = parts(path);
    match p.next() {
        None => false,
        Some("dev") => match p.next() {
            None => false,
            Some(name) => !super::init::default_dev_name(name),
        },
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::mount;
    use super::*;

    fn spec(extra: &str) -> Spec {
        Spec::parse(
            format!(
                r#"{{"ociVersion":"1.0.2","root":{{"path":"rootfs"}},
        "mounts":[{{"destination":"/dev","type":"tmpfs","source":"devices",
        "options":["nosuid","mode=755","size=65536k"{extra}]}}]}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    /// AC-T7.1.4: request different sources/options and readonly/custom-device variants. Expect
    /// exact keys for writable tmpfs, with ineligible specifications left to ordinary setup.
    #[test]
    fn keys_preserve_source_flags_and_data() {
        let s = spec("");
        let (_, a) = eligible(&s, &mount::plan(&s.mounts).unwrap()).unwrap();
        assert_eq!(a.source, "devices");
        assert_eq!(a.data, "mode=755,size=65536k");
        assert_eq!(a.flags, MountFlags::NOSUID);
        let s = spec(",\"strictatime\"");
        let (_, b) = eligible(&s, &mount::plan(&s.mounts).unwrap()).unwrap();
        assert_ne!(a, b);
        let s = spec(",\"ro\"");
        assert!(eligible(&s, &mount::plan(&s.mounts).unwrap()).is_err());
    }

    /// AC-T7.1.4: normalize spellings of targets which could observe early defaults. Expect each
    /// default and the root refused by the optimization, with ordinary child directories allowed.
    #[test]
    fn default_targets_keep_the_original_mount_order() {
        for p in [
            "/",
            "/./",
            "dev",
            "/dev/./null",
            "//dev//fd/4",
            "/dev/ptmx",
            "/dev/shm/../null",
            "/etc/../dev/fd/4",
        ] {
            assert!(!target_unaffected(p), "{p}");
        }
        for p in ["/dev/pts", "/dev/shm", "/dev/mqueue", "/etc/hosts"] {
            assert!(target_unaffected(p), "{p}");
        }
        assert!(is_dev("//dev/./"));
    }

    /// AC-T7.1.4: duplicate /dev, custom devices and explicit inode quotas retain ordinary construction
    /// order; a different source or data option is a different pool key, and a key past the 2,048 bytes
    /// once picked for it is served, bounded by its spec alone.
    #[test]
    fn sensitive_specs_use_ordinary_setup() {
        let mut s = spec("");
        let plan = mount::plan(&s.mounts).unwrap();
        let (_, original) = eligible(&s, &plan).unwrap();
        s.mounts.push(s.mounts[0].clone());
        assert!(eligible(&s, &mount::plan(&s.mounts).unwrap()).is_err());
        let s = spec(",\"nr_inodes=12\"");
        assert!(eligible(&s, &mount::plan(&s.mounts).unwrap()).is_err());
        let mut s = spec("");
        s.linux.devices.push(
            serde_json::from_str(r#"{"path":"/dev/custom","type":"c","major":1,"minor":3}"#)
                .unwrap(),
        );
        assert!(eligible(&s, &mount::plan(&s.mounts).unwrap()).is_err());
        let mut changed = plan.clone();
        changed[0].kind_source = "different".to_owned();
        assert_ne!(eligible(&spec(""), &changed).unwrap().1, original);
        changed[0].data.push_str(",uid=5");
        assert_ne!(eligible(&spec(""), &changed).unwrap().1, original);
        // A key is bounded by the spec it comes from (spec::MAX_CONFIG_BYTES), with no bound of its own: one
        // over the 2,048 bytes once picked for it is served from the pool.
        changed[0].data = "x".repeat(4 * 2048);
        assert!(eligible(&spec(""), &changed).is_ok());
    }

    /// AC-T7.1.4: leave ineligible specifications on ordinary setup and report their fixed reason.
    #[test]
    fn ordinary_setup_has_a_fixed_eligibility_reason() {
        let base = spec("");
        let plan = mount::plan(&base.mounts).unwrap();
        assert_eq!(eligible(&base, &[]), Err(Unpooled::Missing));
        let mut changed = plan.clone();
        changed.push(plan[0].clone());
        assert_eq!(eligible(&base, &changed), Err(Unpooled::Duplicate));
        for bind in [false, true] {
            let mut changed = plan.clone();
            changed[0].is_bind = bind;
            changed[0].fstype = "proc".to_owned();
            assert_eq!(eligible(&base, &changed), Err(Unpooled::Kind));
        }
        let mut changed = plan.clone();
        changed[0].flags |= MountFlags::RDONLY;
        assert_eq!(eligible(&base, &changed), Err(Unpooled::Readonly));
        changed = plan.clone();
        changed[0].data.push_str(",nr_inodes=12");
        assert_eq!(eligible(&base, &changed), Err(Unpooled::Inodes));
        changed = plan;
        changed[0].data = "x".repeat(4 * 2048);
        assert_eq!(
            eligible(&base, &changed).map(|(_, k)| k.data.len()),
            Ok(4 * 2048)
        );
        let mut devices = base;
        devices.linux.devices.push(
            serde_json::from_str(r#"{"path":"/dev/custom","type":"c","major":1,"minor":3}"#)
                .unwrap(),
        );
        assert_eq!(eligible(&devices, &changed), Err(Unpooled::Devices));
    }
}
