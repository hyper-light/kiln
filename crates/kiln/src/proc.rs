//! Child processes, awaited without blocking a shard: `output` (run and capture), `stdout_stream`
//! (stream a child's stdout) and `spawn_group` (a job step in its own process group / job object,
//! killed as one). The implementations live in the platform layer: on Unix the pipes are read as
//! hyper-rt readiness fires and exits arrive through a pidfd (Linux) or a short poll (macOS); on
//! Windows pipes are drained by threads and process trees are job objects.

pub use crate::os::proc::{Detached, Duplex, Running, adopt_duplex, interrupt_group, kill_orphan_group, output, spawn_duplex, spawn_duplex_in, spawn_group, stdout_stream, terminate_group, terminate_tree, wait_pid_exit};

/// How long kiln waits for a program asked its version: a runner's tools (`runner::runtime::memo`)
/// and an upgrade's new kiln (`agent`'s `binary_runs`).
pub const VERSION_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Most of a harness's stdout chunks its [`Duplex`] holds unread, and of its stderr's: the bounds
/// its session's loop reads against, the same for a process of this machine's (`os::proc`) and for
/// a run its boundary holds (`crate::sandbox::held`).
pub const STDOUT_CHUNKS: usize = 256;
pub const STDERR_CHUNKS: usize = 64;

/// [`spawn_duplex_in`] on the blocking pool, never on a shard: joining a group can wait. A group
/// headroom paces is frozen for part of each period, and so is a process that joins it between
/// fork and exec, while `spawn` waits for that exec. On a shard the wait stops everything else the
/// shard serves, its listeners too (the front door stopped answering while one waited).
///
/// Its pipes are served on the caller's shard (goal 10, plan 01a1a87b2dae step 5), where what's read
/// of them is taken (an agent session's task reading its harness's output): spawned from the blocking
/// pool, off every shard, they went to the next shard round-robin, which on a runner of two shards was
/// another thread than the session's for every other harness, a wake across threads for each chunk
/// of its output on its way to the server's viewers.
pub async fn spawn_duplex_in_pool(cmd: std::process::Command, group: Option<crate::os::contain::Group>) -> anyhow::Result<Duplex> {
    let home = crate::rt::shard_index();
    crate::rt::blocking(move || spawn_duplex_in(cmd, group.as_ref(), home)).await?
}

/// A harness a boundary holds (a microVM's member of `image`, reaching what `reach` says of its
/// image's destinations; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17) as the [`Duplex`] a
/// session drives it through: its pipes, end and stop the run's (`crate::sandbox::held`), served on
/// the caller's shard, as a spawned harness's pipes are.
pub fn duplex_of_run(run: Box<dyn kiln_seams::sandbox::Run>, image: String, reach: crate::sandbox::narrow::Reach) -> Duplex {
    Duplex::held(crate::sandbox::held::pump(run, image, reach))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn output_and_stream_and_group() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let mut c = Command::new("sh");
            c.args(["-c", "echo hi; echo err >&2; exit 3"]);
            let out = output(c, Duration::from_secs(10)).await?;
            assert_eq!(out.status.code(), Some(3));
            assert_eq!(out.stdout, b"hi\n");
            assert_eq!(out.stderr, b"err\n");

            // Large output on both pipes at once must not deadlock.
            let mut c = Command::new("sh");
            c.args(["-c", "head -c 3000000 /dev/zero; head -c 2000000 /dev/zero >&2"]);
            let out = output(c, Duration::from_secs(20)).await?;
            assert_eq!((out.stdout.len(), out.stderr.len()), (3_000_000, 2_000_000));

            let mut slow = Command::new("sleep");
            slow.arg("30");
            let t = std::time::Instant::now();
            assert!(output(slow, Duration::from_millis(200)).await.is_err());
            assert!(t.elapsed() < Duration::from_secs(5));

            let mut c = Command::new("sh");
            c.args(["-c", "printf abc; printf def"]);
            let rx = stdout_stream(c)?;
            let mut all = Vec::new();
            while let Ok(b) = rx.recv().await {
                all.extend_from_slice(&b);
            }
            assert_eq!(all, b"abcdef");

            let mut c = Command::new("sh");
            c.args(["-c", "echo started; sleep 30"]);
            let mut running = spawn_group(c)?;
            let first = running.output.recv().await?;
            assert_eq!(first, b"started\n");
            terminate_group(&mut running, Duration::from_secs(5)).await;
            assert_eq!(running.wait().await, None);
            Ok(())
        })
    }
}
