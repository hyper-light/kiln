//! The rootless delete flake (STATUS.md): a box stayed `populated` 60 s after `cgroup.kill` with
//! nothing to kill, and `delete` waited out its patience and refused. One cause, proven on Linux 7.2.9
//! and 6.19 (`bench/competitive/probes/delete_flake.md`): `cgroup.kill` signals thread-group *leaders*,
//! so a thread group whose leader thread has already exited keeps a live non-leader thread that
//! `cgroup.kill` never reaches and that holds the cgroup populated. (Whether this also explains the
//! separate 60 s empty-list stalls is unproven — another observation to evidence, not assume.) This
//! test reproduces the leaderless group — a process in a box's cgroup whose leader thread calls
//! `pthread_exit` while a worker thread lives — and requires `cgroup.kill` then `wait_empty` to empty
//! the box. Without the fix the worker survives and `wait_empty` times out; the fix sweeps the box's
//! `cgroup.procs` and sends each process a process-directed SIGKILL through a process pidfd, which the
//! kernel delivers to the whole thread group and so reaches the live worker the leader-directed
//! `cgroup.kill` missed (`BoxCgroup::kill`, probe `delete_flake_point4.c`). A second test confirms a
//! box's kill spares a process outside it: the sweep signals an id only while its cgroup still lists it
//! when, the pidfd in hand, it re-reads to check — so no outside process, not even one whose id a box
//! process had and the kernel reused, is a candidate. T-T7.1.2, the delete half.
//!
//! It needs a delegated cgroup v2 subtree, as `process_box.rs`: it uses this process's own cgroup
//! where that is private and delegated, else re-runs once under `systemd-run --user --scope
//! -p Delegate=yes`; where neither is possible it prints SKIP rather than pass falsely.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use hex_api::{Bounds, BoxId};
use hex_runtime::linux::cgroup::BoxRoot;

const TEST: &str = "a_leaderless_thread_group_is_killed_on_delete";

/// How long a box's cgroup has to empty after cgroup.kill: the oci class's own delete waits this long for it,
/// `DELETE_PATIENCE` in crates/hex-runtime/src/linux/oci/run.rs (10 s; private there, so named here with its source),
/// so the test holds the kill to the bound the delete it exercises holds it to (Ada's 2026-11-06 rule).
const DELETE_PATIENCE: Duration = Duration::from_secs(10);

#[test]
fn a_leaderless_thread_group_is_killed_on_delete() {
    match prepare_cgroup() {
        Ok(own) => {
            exercise(&own);
            spares_outsiders(&own);
        }
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                eprintln!(
                    "SKIP: own cgroup isn't ours to subdivide ({direct}) and no systemd --user"
                );
                return;
            }
            let test_bin = std::env::current_exe().expect("test binary path");
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(&test_bin)
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed");
        }
        // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the test,
        // never a skip that would hide the breakage.
        Err(direct) => panic!("its cgroup isn't delegated to it ({direct})"),
    }
}

fn exercise(own: &Path) {
    let helper = build_leaderless_helper();
    let root = BoxRoot::open(&own.join("hex-delete-flake"), Vec::new()).expect("open a box root");
    let id = BoxId::new("leaderless").unwrap();
    let cg = root
        .create(&id, &Bounds::default())
        .expect("make the box cgroup");
    // A process in the box whose leader thread exits while a worker lives: a leaderless thread group.
    let procs = cg.dir().join("cgroup.procs");
    let mut child = Command::new(&helper)
        .arg(&procs)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the leaderless helper");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        line.trim(),
        "ready",
        "the helper joined the box and went leaderless"
    );
    assert!(
        cg.populated().unwrap(),
        "the box holds the leaderless thread group"
    );

    // The delete path: cgroup.kill, then empty. cgroup.kill alone leaves the leaderless worker alive
    // (the bug); the fix SIGKILLs it by its thread id. Give it the oci delete's own patience.
    cg.kill().expect("kill the box");
    let emptied = cg
        .wait_empty(DELETE_PATIENCE)
        .expect("wait for the box to empty");
    let left = survivors(cg.dir());
    // Tear down before asserting, so a run that fails (cgroup.kill left the leaderless worker alive)
    // leaks neither the helper nor its cgroup: a process-directed SIGKILL on the child reaches the
    // worker that the leader-directed cgroup.kill missed, and the now-empty cgroup is removed. Without
    // this a failing run would leave a populated cgroup and the next run would hit `Exists`.
    let _ = child.kill();
    let _ = child.wait();
    let _ = cg.remove();
    assert!(
        emptied,
        "the box must empty after cgroup.kill, but a leaderless thread held it: {left}"
    );
}

/// Point 1 of the delete-fix review: a box's kill must spare a process outside the box. The box holds a
/// leaderless group, so the kill takes the freeze-and-sweep path; an outsider runs in the test's own
/// cgroup the whole time, never joining the box, and must still be running after the box empties — the
/// sweep signals only ids in the box's own (frozen) cgroup.procs, so no outsider, not even one whose id
/// a box process had and the kernel reused, is ever a candidate.
fn spares_outsiders(own: &Path) {
    let helper = build_leaderless_helper();
    let root = BoxRoot::open(&own.join("hex-delete-spare"), Vec::new()).expect("open a box root");
    let id = BoxId::new("spare").unwrap();
    let cg = root
        .create(&id, &Bounds::default())
        .expect("make the box cgroup");
    // An outsider: a child left in the test's own cgroup, never moved into the box.
    let mut outsider = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn an outside process");
    // The box's leaderless group, so the kill must freeze and sweep rather than stop at cgroup.kill.
    let procs = cg.dir().join("cgroup.procs");
    let mut child = Command::new(&helper)
        .arg(&procs)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the leaderless helper");
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        line.trim(),
        "ready",
        "the box holds the leaderless thread group"
    );

    cg.kill().expect("kill the box");
    let emptied = cg
        .wait_empty(DELETE_PATIENCE)
        .expect("wait for the box to empty");
    // Still running? try_wait is None while the child lives, so the box's kill did not reach it.
    let outsider_alive = matches!(outsider.try_wait(), Ok(None));
    // Tear down before asserting, so a failing run leaks neither child nor the cgroup.
    let _ = child.kill();
    let _ = child.wait();
    let _ = outsider.kill();
    let _ = outsider.wait();
    let _ = cg.remove();
    assert!(emptied, "the box with the leaderless group must empty");
    assert!(
        outsider_alive,
        "a process outside the box must survive the box's kill"
    );
}

/// Every thread id still in the box's cgroup subtree, each with its state — what held the box when it
/// didn't empty, so a failure names it.
fn survivors(root: &Path) -> String {
    let mut out = String::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.threads")) {
            for tid in text
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .take(16)
            {
                let st = std::fs::read_to_string(format!("/proc/{tid}/stat")).unwrap_or_default();
                out.push_str(&format!(
                    " [tid {tid}: {}]",
                    st.split(')').next_back().unwrap_or("").trim()
                ));
            }
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path().is_dir() {
                    queue.push(e.path());
                }
            }
        }
    }
    if out.is_empty() {
        " (nothing listed)".to_owned()
    } else {
        out
    }
}

/// Compiles the leaderless-thread-group helper and returns its path. `cc <procs>`: it joins the box's
/// cgroup (writes `0` to the `cgroup.procs` path it is given), starts a worker thread, and its main
/// (leader) thread calls `pthread_exit`, so the process lives on leaderless. The worker says `ready`
/// only once it has seen the leader become a zombie — the fact the test waits on, not a sleep — so the
/// group is provably leaderless before the test kills it. Without that, under load the leader could
/// still be alive at kill time, `cgroup.kill` would reach it and empty the box, and the test would pass
/// with no fix (a vacuous pass); waiting on the zombie makes the no-fix run fail every time.
fn build_leaderless_helper() -> PathBuf {
    let dir = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/boxtest");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("leaderless.c");
    let bin = dir.join("leaderless");
    std::fs::write(
        &src,
        r#"#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static int leader_tid;
/* 1 once the leader thread (its tid == the pid) is a zombie: it pthread_exit'd while this worker
   lives, so the thread group is leaderless, which is what cgroup.kill misses. 1 too if the task is
   already gone. The state is the char two past the last ')' in /proc/<tid>/task/<tid>/stat. */
static int leader_gone(void){
    char path[64], buf[256];
    snprintf(path, sizeof path, "/proc/%d/task/%d/stat", leader_tid, leader_tid);
    int fd = open(path, O_RDONLY); if(fd < 0) return 1;
    ssize_t n = read(fd, buf, sizeof buf - 1); close(fd);
    if(n <= 0) return 1; buf[n] = 0;
    char *rp = strrchr(buf, ')');
    return rp && rp[1] == ' ' && rp[2] == 'Z';
}
static void *worker(void *a){ (void)a;
    /* wait on the fact, not a clock: the leader has exited and we are the lone live thread. Bounded. */
    int i; for(i = 0; i < 1000000 && !leader_gone(); i++) sched_yield();
    if(i >= 1000000){ fprintf(stderr, "leader did not become a zombie\n"); _exit(3); }
    printf("ready\n"); fflush(stdout);
    for(;;) pause(); return 0;
}
int main(int argc, char **argv){
    if(argc < 2){ fprintf(stderr, "usage: leaderless <cgroup.procs>\n"); return 2; }
    int fd = open(argv[1], O_WRONLY); if(fd < 0){ perror("open cgroup.procs"); return 1; }
    if(write(fd, "0", 1) != 1){ perror("write cgroup.procs"); return 1; } close(fd);
    leader_tid = getpid();   /* the leader thread's tid == the pid */
    pthread_t t; if(pthread_create(&t, 0, worker, 0)){ perror("pthread_create"); return 1; }
    pthread_exit(0);   /* the leader exits; the worker prints ready only once it sees the zombie */
}
"#,
    )
    .unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let ok = Command::new(&cc)
        .args(["-O2", "-pthread", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("compiling the leaderless helper with {cc}: {e}"))
        .success();
    assert!(ok, "the leaderless helper did not compile");
    bin
}

/// This process's own cgroup, delegated to it and prepared once for every test in this binary
/// (`hex_testkit::cgroup::delegated`: the process and every process of its own moved into a leaf), so a sibling leaf
/// can hold the box; `Err` with the reason when the cgroup isn't ours to subdivide. A probe box root proves it is, as
/// the other cgroup tests' do: opening one hands the cgroup's controllers down, which cgroup v2 refuses while the
/// cgroup still holds processes not this one's (a delegated systemd unit's shell beside this test), and that refusal
/// re-runs the test in a scope of its own rather than failing in `exercise`.
fn prepare_cgroup() -> Result<PathBuf, String> {
    hex_testkit::cgroup::delegated(|own| {
        let probe = BoxRoot::open(&own.join("hex-probe"), Vec::new())
            .map(|_| ())
            .map_err(|e| e.to_string());
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        probe
    })
}
