//! Child processes, awaited without blocking a shard: `output` (run and capture), `stdout_stream`
//! (stream a child's stdout) and `spawn_group` (a job step in its own process group / job object,
//! killed as one). The implementations live in the platform layer: on Unix the pipes are read as
//! hyper-rt readiness fires and exits arrive through a pidfd (Linux) or a short poll (macOS); on
//! Windows pipes are drained by threads and process trees are job objects.

pub use crate::os::proc::{Detached, Duplex, Running, adopt_duplex, interrupt_group, kill_orphan_group, output, spawn_duplex, spawn_duplex_in, spawn_group, stdout_stream, terminate_group, terminate_tree, wait_pid_exit};

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
