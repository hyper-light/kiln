//! A capacity holder's scope as the host reads it (joint plan step 13, CV12; AC-T7.4.1).

use super::*;

/// AC-T7.4.1, CV12: a cgroup tree under a stand-in hierarchy, owned by this test's user, its scope's
/// `memory.current` 3 GiB; read it for this user, for another, by paths that leave the hierarchy or are empty, one
/// that doesn't exist, and once its count isn't a number. Expect 3 GiB, then NotOwned, Invalid three times, and
/// Unreadable twice.
#[cfg(unix)]
#[test]
fn cv12_a_cgroup_is_read_only_once_proved_its_holders_users() {
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::tempdir().unwrap();
    let jobs = root.path().join("kiln.slice/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("memory.current"), format!("{}\n", 3u64 << 30)).unwrap();
    let me = std::fs::metadata(&jobs).unwrap().uid();
    assert_eq!(cgroup(root.path(), "kiln.slice/jobs", me), Ok(3 << 30));
    assert_eq!(
        cgroup(root.path(), "kiln.slice/jobs", me.wrapping_add(1)),
        Err(ScopeRefused::NotOwned),
        "a cgroup another user owns was credited to this one"
    );
    for leaving in ["../etc", "/etc", ""] {
        assert_eq!(
            cgroup(root.path(), leaving, me),
            Err(ScopeRefused::Invalid),
            "{leaving:?}"
        );
    }
    assert_eq!(
        cgroup(root.path(), "kiln.slice/gone", me),
        Err(ScopeRefused::Unreadable)
    );
    std::fs::write(jobs.join("memory.current"), "lots\n").unwrap();
    assert_eq!(
        cgroup(root.path(), "kiln.slice/jobs", me),
        Err(ScopeRefused::Unreadable)
    );
}

/// AC-T7.4.1, CV12: SIDs in their string forms against binary ones: a user's against its own and against
/// BUILTIN\Administrators', which owns a job an elevated holder makes, and malformed ones. Expect only its own to
/// match, so an elevated runner's job earns no credit (the safe outcome), and each malformed one to name no SID.
#[test]
fn cv12_a_job_is_its_holders_only_when_its_owner_is_the_declared_sid() {
    let user = "S-1-5-21-1004336348-1177238915-682003330-1001";
    let own = sid_bytes(user).unwrap();
    assert_eq!(own.len(), 8 + 4 * 5);
    assert_eq!(own[..8], [1, 5, 0, 0, 0, 0, 0, 5]);
    assert!(sid_is(&own, user));
    let administrators = sid_bytes("S-1-5-32-544").unwrap();
    assert_eq!(
        administrators,
        [1, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 0x20, 2, 0, 0]
    );
    assert!(
        !sid_is(&administrators, user),
        "a job BUILTIN\\Administrators owns passed as the holder's own"
    );
    assert_eq!(
        sid_bytes("S-1-0x000000000005-32-544").unwrap(),
        administrators
    );
    for bad in [
        "",
        "S-1-",
        "S-2-5-32",
        "S-1-5",
        "S-1-x-1",
        "S-1-5-32-544-",
        "S-1-0x12-1",
    ] {
        assert!(sid_bytes(bad).is_none(), "{bad:?}");
    }
}

/// AC-T7.4.1, CV12: read two holders' scopes around a read, one with no user declared and one of a kind no OS
/// accounts here; expect the read made once between, and each refused, typed: NoUser, and NotThisPlatform.
#[test]
fn cv12_scopes_are_read_around_the_free_memorys_read_and_refused_typed() {
    let other = if cfg!(windows) {
        Scope::Group(1)
    } else {
        Scope::Job("kiln-jobs".into())
    };
    let holders = [
        (1, Scope::Cgroup("kiln.slice".into()), None),
        (2, other, Some(User("1000".into()))),
    ];
    let mut reads = 0;
    let (_, sample) = around(&holders, || reads += 1);
    assert_eq!(reads, 1);
    assert_eq!(
        sample,
        vec![
            (1, Err(ScopeRefused::NoUser)),
            (2, Err(ScopeRefused::NotThisPlatform))
        ]
    );
}

/// AC-T7.4.1, CV12: this test's own process group, read for this test's user, for another user, and a group id
/// that can't be one; expect a footprint, NotOwned, and Invalid.
#[cfg(target_os = "macos")]
#[test]
fn cv12_a_process_group_is_read_only_once_every_member_runs_as_the_holders_user() {
    let group = rustix::process::getpgrp().as_raw_nonzero().get();
    let me = rustix::process::getuid().as_raw();
    let used = |uid: u32| super::used(&Scope::Group(group), Some(&User(uid.to_string())));
    assert!(used(me).unwrap().mem_bytes > 0);
    assert_eq!(used(me.wrapping_add(1)), Err(ScopeRefused::NotOwned));
    assert_eq!(
        super::used(&Scope::Group(0), Some(&User(me.to_string()))),
        Err(ScopeRefused::Invalid)
    );
}

/// AC-T7.4.1, CV12: a job this test makes, read for its owner's SID and for another's; expect its resident memory
/// (none: no process is in it) and NotOwned. Run on Windows (jupiter, the lead's).
#[cfg(windows)]
#[test]
fn cv12_a_job_is_read_only_once_its_owner_is_the_holders_account() {
    let name = format!("hex-scope-test-{}", std::process::id());
    let _job = hex_runtime::windows::sys::Job::create_named(&name).unwrap();
    let owner = hex_runtime::windows::sys::Job::open_to_read(&name)
        .unwrap()
        .unwrap()
        .owner()
        .unwrap();
    let text = sid_text(&owner);
    assert!(sid_is(&owner, &text), "{text}");
    let scope = Scope::Job(name);
    assert_eq!(super::used(&scope, Some(&User(text))), Ok(Amounts::ZERO));
    let other = if sid_is(&owner, "S-1-5-32-544") {
        "S-1-5-18"
    } else {
        "S-1-5-32-544"
    };
    assert_eq!(
        super::used(&scope, Some(&User(other.into()))),
        Err(ScopeRefused::NotOwned)
    );
}

/// A SID's string form from its binary one (MS-DTYP 2.4.2.1), for the Windows test to declare its job's owner.
#[cfg(windows)]
fn sid_text(sid: &[u8]) -> String {
    let count = usize::from(sid[1]);
    let mut authority = [0u8; 8];
    authority[2..].copy_from_slice(&sid[2..8]);
    let mut text = format!("S-1-{}", u64::from_be_bytes(authority));
    for i in 0..count {
        let at = 8 + 4 * i;
        let sub = u32::from_le_bytes(sid[at..at + 4].try_into().unwrap());
        text.push_str(&format!("-{sub}"));
    }
    text
}
