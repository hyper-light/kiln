//! PTY plumbing on Windows (ConPTY through portable-pty). The pseudoconsole's pipes cannot be
//! awaited by readiness, so output is drained by one thread and input written by another; the
//! shell's exit is a 50 ms `try_wait` poll. Closing a pseudoconsole can wait for conhost to
//! flush, so masters are closed off the shards.

use std::io::{Read as _, Write as _};
use std::time::Duration;

use portable_pty::{Child, MasterPty};

use crate::terminal::PtyOut;

/// The I/O side of a PTY session: the pseudoconsole's output reader and input writer.
pub struct PtyIo {
    reader: Box<dyn std::io::Read + Send>,
    writer: Box<dyn std::io::Write + Send>,
}

impl PtyIo {
    /// Take hold of the master's I/O. Called before the child is spawned.
    pub fn new(master: &dyn MasterPty) -> anyhow::Result<Self> {
        Ok(Self { reader: master.try_clone_reader()?, writer: master.take_writer()? })
    }

    /// Pump PTY output (then the exit status) into `out`, and `input` into the PTY.
    pub fn start(self, mut child: Box<dyn Child + Send + Sync>, out: async_channel::Sender<PtyOut>, input: async_channel::Receiver<Vec<u8>>) {
        let Self { mut reader, mut writer } = self;
        // PTY → channel. Keeps draining after the consumer is gone: an undrained pseudoconsole
        // blocks both the shell and ClosePseudoConsole.
        let data = out.clone();
        let spawned = std::thread::Builder::new().name("kiln-pty-read".into()).spawn(move || {
            let mut buf = vec![0u8; 32 * 1024];
            let mut wanted = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if wanted {
                            wanted = data.send_blocking(PtyOut::Data(buf.get(..n).map(<[u8]>::to_vec).unwrap_or_default())).is_ok();
                        }
                    }
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the PTY reader thread");
        }
        // Channel → PTY.
        let spawned = std::thread::Builder::new().name("kiln-pty-write".into()).spawn(move || {
            while let Ok(bytes) = input.recv_blocking() {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the PTY writer thread");
        }
        // The exit status, after the last output has had a moment to drain.
        crate::rt::spawn(async move {
            let code = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break i32::try_from(st.exit_code()).ok(),
                    Ok(None) => crate::rt::sleep(Duration::from_millis(50)).await,
                    Err(_) => break None,
                }
            };
            crate::rt::sleep(Duration::from_millis(200)).await;
            let _ = out.send(PtyOut::Exit(code)).await;
        });
    }
}

/// Close a pseudoconsole on a thread of its own: `ClosePseudoConsole` may wait for conhost.
pub fn close_master(master: Box<dyn MasterPty + Send>) {
    if let Err(e) = std::thread::Builder::new().name("kiln-pty-close".into()).spawn(move || drop(master)) {
        tracing::debug!(error = %e, "closing the PTY inline");
    }
}
