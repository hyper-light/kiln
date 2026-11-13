use super::*;

fn config() -> GuestConfig {
    GuestConfig {
        binary: "/model/shards".into(),
        kernel: "/model/kernel".into(),
        initrd: "/model/initrd".into(),
        vcpus: 3,
        memory_mib: 256,
        gpu: false,
        staged: "/model/staged".into(),
        shares: vec![
            Share {
                dir: "/model/read-only".into(),
                read_only: true,
                at: "/model/guest/0".into(),
            },
            Share {
                dir: "/model/write".into(),
                read_only: false,
                at: "/model/guest/1".into(),
            },
        ],
    }
}
fn encoded() -> Vec<OsString> {
    let config = config();
    let paths = RuntimePaths::new("/model/cache".into(), "/model/home".into()).unwrap();
    arguments(
        native_fixture::Input {
            config: &config,
            cost_file: Path::new("/model/cost"),
            cost_sha256: [0x33; 32],
            evidence: Path::new("/model/evidence"),
            paths: &paths,
            socket: Path::new("/model/socket"),
            console_kept: 0,
        },
        Path::new("/model/hex"),
        [0x11; 16],
        [0x22; 32],
        Duration::new(17, 19),
        Clamp::Unclamped,
    )
    .unwrap()
}

/// AC-T7.4.1: process exec transports owned bytes, not a pointer or a reparsed
/// foreign command. Ordered share rights, explicit kept0 and original expiry survive.
#[test]
fn fixed_carrier_preserves_original_typed_rights_and_deadline() {
    let mut config = config();
    config.kernel = PathBuf::from(OsString::from_vec(b"/model/non-utf8-\xff".to_vec()));
    let paths = RuntimePaths::new("/model/cache".into(), "/model/home".into()).unwrap();
    let args = arguments(
        native_fixture::Input {
            config: &config,
            cost_file: Path::new("/model/cost"),
            cost_sha256: [0x33; 32],
            evidence: Path::new("/model/evidence"),
            paths: &paths,
            socket: Path::new("/model/socket"),
            console_kept: 0,
        },
        Path::new("/model/hex"),
        [0x11; 16],
        [0x22; 32],
        Duration::new(17, 19),
        Clamp::Background,
    )
    .unwrap();
    let decoded = parse(args).unwrap();
    assert_eq!(decoded.config, config);
    assert_eq!(decoded.paths, paths);
    assert_eq!(decoded.console_kept, 0);
    assert_eq!(decoded.expiry, Duration::new(17, 19));
    assert_eq!(decoded.scope, [0x11; 16]);
    assert_eq!(decoded.commitment, [0x22; 32]);
    assert_eq!(decoded.clamp, Clamp::Background);
    assert_eq!(decoded.cost_sha256, [0x33; 32]);
}

/// Hostile fixed input is refused before the sole fixture builder/file/native call.
/// Paths below are syntax only; this unit creates/dials no file or socket.
#[test]
fn malformed_carrier_cannot_select_defaults_or_gain_extra_rights() {
    for (at, value) in [
        (8, "03"),
        (10, "2"),
        (17, "1000000000"),
        (18, "short"),
        (20, "2"),
        (21, "31"),
    ] {
        let mut args = encoded();
        args[at] = value.into();
        assert!(parse(args).is_err(), "field {at}");
    }
    let mut args = encoded();
    args[2] = "relative/cost".into();
    assert!(parse(args).is_err());
    let mut args = encoded();
    args.push("extra".into());
    assert!(parse(args).is_err());
    let mut args = encoded();
    args.pop();
    assert!(parse(args).is_err());
    let mut args = encoded();
    args[23] = "2".into();
    assert!(parse(args).is_err(), "share mode is explicit");
    let mut args = encoded();
    args[1] = "__other".into();
    assert!(parse(args).is_err());
}

fn report(pid: i32, birth: u64, scope: [u8; 16], commitment: [u8; 32]) -> String {
    format!(
        "ENTER {pid} {birth}\nKEY {} {}\nNPROC 0 0\nFD 0 3 1 0\nFD 1 4 0 0\nFD 2 5 1 0\n",
        hex(&scope).unwrap().to_str().unwrap(),
        hex(&commitment).unwrap().to_str().unwrap()
    )
}
/// A stale same-PID/birth report cannot certify another scope/commitment; exit0
/// and marker-only output also cannot substitute for positive native setup facts.
#[test]
fn report_requires_exact_identity_scope_seal_and_unique_fd_observations() {
    let raw = report(77, 88, [0x11; 16], [0x22; 32]);
    report_nproc(raw.as_bytes(), 77, 88, [0x11; 16], [0x22; 32]).unwrap();
    for (pid, birth, scope, commitment) in [
        (78, 88, [0x11; 16], [0x22; 32]),
        (77, 89, [0x11; 16], [0x22; 32]),
        (77, 88, [0x12; 16], [0x22; 32]),
        (77, 88, [0x11; 16], [0x23; 32]),
    ] {
        assert!(report_nproc(raw.as_bytes(), pid, birth, scope, commitment).is_err());
    }
    for changed in [
        raw.replace("NPROC 0 0", "NPROC 0 1"),
        raw.replace("FD 2 5 1 0", "FD 2 3 1 0"),
        raw.replace("FD 2 5 1 0", "FD 1 5 1 0"),
        raw.replace("FD 2 5 1 0", "FD 2 5 1 1"),
        raw.replace("FD 2 5 1 0\n", ""),
        format!("{raw}NPROC 0 0\n"),
        format!("{raw}EXECERR native error\n"),
        "Code0\n".into(),
    ] {
        assert!(report_nproc(changed.as_bytes(), 77, 88, [0x11; 16], [0x22; 32]).is_err());
    }
}

/// Arg/env pointer+NUL overhead is part of the existing ARG_MAX, including on
/// the exact boundary. Extra fields cannot hide behind a short payload.
#[test]
fn native_exec_budget_counts_pointer_arrays_and_both_terminators() {
    let args = vec![OsString::from("/model/hex"), OsString::from("abc")];
    let env = vec![CString::new("A=B").unwrap()];
    let maximum = (args.len() + env.len() + 2) * size_of::<usize>()
        + args.iter().map(|a| a.len() + 1).sum::<usize>()
        + 4;
    check_exec_bytes(&args, &env, maximum).unwrap();
    assert!(check_exec_bytes(&args, &env, maximum - 1).is_err());
    let mut extra = env;
    extra.push(CString::new("C=D").unwrap());
    assert!(check_exec_bytes(&args, &extra, maximum).is_err());
}

/// A failed diagnostic never overwrites an earlier native/publisher failure or
/// a cleanup error. These are model errors, not a native cleanup result.
#[test]
fn bounded_error_record_preserves_primary_cleanup_and_raw_write_failure() {
    let mut details = Vec::new();
    details.try_reserve_exact(1).unwrap();
    let capacity = details.capacity();
    details.push(FailureDetails {
        primary: Fault::Returned(91),
        cleanup: Some(Fault::Native(native::Error::Deadline)),
        diagnostic: [
            Some(io(
                "recording receipt",
                std::io::Error::from_raw_os_error(libc::ENOSPC),
            )),
            None,
            None,
        ],
        receipt: None,
        returned: None,
    });
    assert_eq!(details.capacity(), capacity);
    assert_eq!(details.len(), 1);
    let failure = Failure { details };
    let text = failure.to_string();
    assert!(text.contains("91"));
    assert!(matches!(
        failure.details[0].cleanup.as_ref(),
        Some(Fault::Native(native::Error::Deadline))
    ));
    assert!(text.contains("recording receipt"));
    assert!(
        matches!(failure.details[0].diagnostic[0],Some(Fault::Io{ref source,..}) if source.raw_os_error()==Some(libc::ENOSPC))
    );
}
