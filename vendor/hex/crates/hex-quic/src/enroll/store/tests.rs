use super::*;
use crate::enroll::Ca;
use crate::enroll::sim::SimCa;

/// A folder of the test's own: under ~/.cache on Unix (the hazards' rule: never /tmp), the system's temp folder on
/// Windows, as Hex's Windows tests take.
fn scratch(prefix: &str) -> tempfile::TempDir {
    #[cfg(unix)]
    {
        let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/quic");
        std::fs::create_dir_all(&cache).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&cache)
            .unwrap()
    }
    #[cfg(windows)]
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

/// `name`'s enrollment begun in `dir` and answered by `ca`.
fn enrolled(ca: &SimCa, dir: &Path, name: &str) -> Issued {
    begin(dir, name).unwrap();
    ca.admit(name, b"grant");
    ca.enroll(&request(dir, b"grant".to_vec()).unwrap())
        .unwrap()
}

/// HEX-ENROLL-8 (Q6, Q9): an enrollment kept in a folder: its key made there for its owner alone, its request naming
/// the peer, the CA's answer kept once it proves to be for them, and the credentials loaded from the folder the key's,
/// which a link's TLS takes.
#[test]
fn hex_enroll_8_an_enrollment_kept_in_a_folder_loads_as_the_peers_credentials() {
    let ca = SimCa::new().unwrap();
    let dir = scratch("enroll-");
    let path = begin(dir.path(), "node-8.test").unwrap();
    assert_eq!(path, dir.path().join(REQUEST));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(KEY))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
    let asked = request(dir.path(), b"grant".to_vec()).unwrap();
    assert_eq!(asked.name, "node-8.test");
    ca.admit("node-8.test", b"grant");
    let issued = ca.enroll(&asked).unwrap();
    finish(dir.path(), &issued).unwrap();
    let credentials = load(dir.path()).unwrap();
    assert_eq!(credentials.chain, issued.chain);
    assert_eq!(credentials.roots, issued.roots);
    let kept = key(dir.path()).unwrap();
    assert_eq!(
        credentials.key.secret_der(),
        kept.0.serialize_der().as_slice()
    );
    assert!(crate::tls::tls(&credentials, true).is_ok());
}

/// HEX-ENROLL-9 (Q6, Q9): what isn't an enrollment's own is refused, typed, naming its file: a second beginning in
/// the same folder; credentials before the CA's answer is kept; an answer for another key, of which nothing is kept;
/// and a chain another enrollment's key holds.
#[test]
fn hex_enroll_9_a_folder_that_isnt_one_enrollments_is_refused_typed() {
    let ca = SimCa::new().unwrap();
    let mine = scratch("enroll-");
    begin(mine.path(), "node-9.test").unwrap();
    assert!(matches!(
        begin(mine.path(), "node-9.test"),
        Err(Refused::Configuration { .. })
    ));
    let early = load(mine.path()).unwrap_err();
    assert!(
        matches!(&early, Refused::Configuration { why, .. } if why.contains(CHAIN)),
        "{early}"
    );
    let theirs = scratch("enroll-");
    let issued = enrolled(&ca, theirs.path(), "node-9.test");
    assert!(matches!(
        finish(mine.path(), &issued),
        Err(Refused::Issued { .. })
    ));
    assert!(!mine.path().join(CHAIN).exists());
    finish(theirs.path(), &issued).unwrap();
    for file in [CHAIN, ROOTS] {
        std::fs::copy(theirs.path().join(file), mine.path().join(file)).unwrap();
    }
    let crossed = load(mine.path()).unwrap_err();
    assert!(
        matches!(&crossed, Refused::Configuration { why, .. } if why.contains("another key")),
        "{crossed}"
    );
}
