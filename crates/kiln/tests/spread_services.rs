//! Goal 9, plan 01a1a87b2cec step 3: the services a process starts and keeps (supervised children,
//! guarded tasks) go round-robin over the shards, not all on the one that starts them, as a
//! server's start-up on its first shard left every one of them there (the lab's calm baseline: that
//! shard busy 0.24 of the time against 0.016 on each of the others). A process of its own: the
//! runtime is the process's.

use std::collections::BTreeSet;
use std::time::Duration;

/// Services started, from a task on one shard.
const SERVICES: usize = 16;

#[test]
fn g9_s3_services_started_on_one_shard_spread_over_all() -> anyhow::Result<()> {
    kiln::rt::configure(kiln::rt::Profile { shards: 4, ..kiln::rt::Profile::tool() });
    anyhow::ensure!(kiln::rt::shard_count() == 4, "{} shards", kiln::rt::shard_count());
    let (tx, rx) = std::sync::mpsc::channel::<Option<u16>>();
    let reg = kiln::sup::Registry::default();
    let stop = kiln::rt::Shutdown::new();
    // Every service started by one task on shard 0, half supervised and half guarded: each says
    // which shard it runs on, then ends.
    anyhow::ensure!(
        kiln::rt::spawn_on(0, async move {
            for i in 0..SERVICES {
                let tx = tx.clone();
                if i % 2 == 0 {
                    // Dropping a handle leaves its task running.
                    drop(kiln::sup::spawn(&reg, "a service", stop.clone(), move || {
                        let tx = tx.clone();
                        async move {
                            let _ = tx.send(hyper_rt::futures::shard_id().map(|s| s.0));
                            Ok(())
                        }
                    }));
                } else {
                    drop(kiln::sup::spawn_guarded("a guarded task", async move {
                        let _ = tx.send(hyper_rt::futures::shard_id().map(|s| s.0));
                        Ok(())
                    }));
                }
            }
        }),
        "shard 0 refused the starting task"
    );
    let mut on = BTreeSet::new();
    for _ in 0..SERVICES {
        on.insert(rx.recv_timeout(Duration::from_secs(20))?.ok_or_else(|| anyhow::anyhow!("a service ran off the shards"))?);
    }
    anyhow::ensure!(on.len() == 4, "{SERVICES} services started on one shard ran on shards {on:?}, not on all 4");
    Ok(())
}
