//! T-G2.7.1: the gate's run of a pool writes what bench/corpus/subset.py ranks the pool from: each
//! record's outcome and time, and each file's whole time; fill files stop at the budget's deadline.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use std::process::Command;

const FILE: &str = "statement ok
CREATE TABLE t(a INTEGER)

statement ok
INSERT INTO t VALUES (1), (2)

query I rowsort
SELECT a FROM t
----
1
2
";

/// Do: run a pool of one required file, whose last query can't finish, and one fill file, with a
/// budget of one second. Expect: the required file's records agree but the last, which the engine
/// stops at the budget and is classified past it (failing the run: exit 1); its end is past the
/// run's deadline, so the fill file's records are unreached; every record has a times line, and
/// each file has one `file_ns` line saying whether it was required, how many settles failed (none)
/// and how many records were stopped (T-G2.7.1, T-G2.7.4).
#[test]
fn t_g2_7_1_times_name_each_record_and_each_file() {
    let pool = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("amphora-corpus-times-{}", std::process::id()));
    std::fs::create_dir_all(pool.join("a")).unwrap();
    // A 10-way join of 100 rows: 10^20 rows to count, which no machine finishes.
    let values: Vec<String> = (1..=100).map(|i| format!("({i})")).collect();
    let from: Vec<String> = (0..10).map(|i| format!("n t{i}")).collect();
    let endless = format!(
        "\nstatement ok\nCREATE TABLE n(x INTEGER)\n\nstatement ok\nINSERT INTO n VALUES {}\n\nquery I nosort\nSELECT count(*) FROM {}\n----\n0\n",
        values.join(", "),
        from.join(", ")
    );
    std::fs::write(pool.join("a/req.test"), format!("{FILE}{endless}")).unwrap();
    std::fs::write(pool.join("a/fill.test"), FILE).unwrap();
    std::fs::write(
        pool.join("order.tsv"),
        "required\ta/req.test\nfill\ta/fill.test\n",
    )
    .unwrap();
    let times = pool.join("times.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_amphora-corpus"))
        .arg("run")
        .arg(&pool)
        .args(["--budget-s", "1", "--times"])
        .arg(&times)
        .output()
        .unwrap();
    let text = std::fs::read_to_string(&times).unwrap();
    std::fs::remove_dir_all(&pool).unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let of = |file: &str, key: &str| -> Vec<String> {
        rows.iter()
            .filter(|r| r["file"] == file && r.get(key).is_some())
            .map(|r| r[key].to_string())
            .collect()
    };
    assert_eq!(
        of("a/req.test", "outcome"),
        ["\"agree\""; 5]
            .into_iter()
            .chain(["\"past_budget\""])
            .collect::<Vec<_>>()
    );
    assert_eq!(of("a/fill.test", "outcome"), ["\"unreached\""; 3]);
    let whole: Vec<_> = rows.iter().filter(|r| r.get("file_ns").is_some()).collect();
    assert_eq!(whole.len(), 2);
    assert_eq!(whole[0]["file"], "a/req.test");
    assert_eq!(whole[0]["required"], true);
    assert_eq!(whole[1]["file"], "a/fill.test");
    assert_eq!(whole[1]["required"], false);
    assert!(whole.iter().all(|w| w["unsettled"] == 0));
    assert_eq!(whole[0]["past_budget"], 1);
    assert_eq!(whole[1]["past_budget"], 0);
}
