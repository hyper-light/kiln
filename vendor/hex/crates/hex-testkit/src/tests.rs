//! The skip record's tests (HEX-GATE-1): what a skip writes, that skips from many threads land whole,
//! the bound on a line, where the record is, and that the gate prints the record it names.

use super::*;

/// Reads the record at `path` as its lines' fields.
fn fields(path: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| l.split('\t').map(str::to_owned).collect())
        .collect()
}

/// HEX-GATE-1: a skip appends one line naming the binary, the test, what it lacked and why, its tabs
/// and line breaks made spaces, and never truncates the record. Do: append two skips, the second's
/// reason with a tab and a newline in it. Expect: two lines of four fields, as written but for those.
#[test]
fn a_skip_appends_one_line_naming_the_test_what_it_lacked_and_why() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(RECORD);
    append(
        &path,
        &line("bin", "first", "cgroup v2 delegation", "no systemd"),
    )
    .unwrap();
    append(&path, &line("bin", "second", "CAP_SYS_ADMIN", "a\tb\nc")).unwrap();
    assert_eq!(
        fields(&path),
        [
            ["bin", "first", "cgroup v2 delegation", "no systemd"],
            ["bin", "second", "CAP_SYS_ADMIN", "a b c"],
        ]
    );
}

/// HEX-GATE-1: skips from tests running at once land whole, none lost or torn. Do: append 100 skips
/// from each of 8 threads. Expect: 800 lines, each of four fields, each skip once.
#[test]
fn skips_from_many_threads_each_land_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(RECORD);
    std::thread::scope(|s| {
        for t in 0..8 {
            let path = &path;
            s.spawn(move || {
                for n in 0..100 {
                    let why = format!("{t}-{n} {}", "x".repeat(200));
                    append(path, &line("bin", "test", "missing", &why)).unwrap();
                }
            });
        }
    });
    let lines = fields(&path);
    assert_eq!(lines.len(), 800);
    let mut seen: Vec<&str> = lines
        .iter()
        .map(|f| {
            assert_eq!(f.len(), 4, "{f:?}");
            f[3].split(' ').next().unwrap()
        })
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 800, "a skip landed twice or not at all");
}

/// HEX-GATE-1: a reason past the bound is cut at a character boundary and the cut marked, so the line
/// stays within it. Do: a reason of 10,000 two-byte characters. Expect: a line within MAX_LINE, ending
/// in the mark and a newline, the reason a prefix of the one given.
#[test]
fn a_long_reason_is_cut_to_the_bound_at_a_character_boundary() {
    let why = "é".repeat(10_000);
    let line = line("bin", "test", "missing", &why);
    assert!(line.len() <= MAX_LINE, "{} bytes", line.len());
    assert!(line.len() > MAX_LINE - 8, "cut short: {} bytes", line.len());
    let reason = line
        .strip_suffix("…\n")
        .unwrap()
        .rsplit('\t')
        .next()
        .unwrap();
    assert!(why.starts_with(reason));
}

/// HEX-GATE-1: the record is `HEX_SKIPS` when the gate sets it, else RECORD under the target
/// directory this test binary sits in.
#[test]
fn the_record_is_the_gates_or_under_the_target_directory() {
    let record = record().unwrap();
    match std::env::var_os(VAR) {
        Some(set) => assert_eq!(record, PathBuf::from(set)),
        None => {
            assert!(record.ends_with(RECORD), "{}", record.display());
            let target = record.parent().unwrap().parent().unwrap();
            assert!(
                target.join("CACHEDIR.TAG").is_file(),
                "{}",
                target.display()
            );
        }
    }
}

/// HEX-GATE-1: the binary's name comes without cargo's hash.
#[test]
fn the_binary_is_named_without_its_hash() {
    assert_eq!(binary(), "hex_testkit");
}

/// Doc-truth (HEX-GATE-1): scripts/gates.sh points HEX_SKIPS at RECORD, empties it before
/// `cargo test` and prints it after.
#[test]
fn the_gate_prints_the_record_after_the_tests() {
    let gates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/gates.sh");
    let gates = fs::read_to_string(gates).unwrap();
    let at = |what: &str| {
        gates
            .find(what)
            .unwrap_or_else(|| panic!("gates.sh lacks {what:?}"))
    };
    assert!(gates.contains(&format!("{VAR}=")));
    assert!(gates.contains(RECORD));
    let tests = at("gate cargo test --workspace --locked");
    assert!(at("rm -f -- \"${HEX_SKIPS:?}\"") < tests);
    assert!(at("cat -- \"$HEX_SKIPS\"") > tests);
}
