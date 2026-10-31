//! U03/W17-R01: portable-pty runs only after the existing kernel launcher has placed this
//! adapter in the borrowed execution's domain. Its children inherit that domain on every OS.
//! No job identity, grant or containment can be supplied through this private pipe protocol.

use std::sync::Arc;

use anyhow::Context as _;
use futures::FutureExt as _;
use serde::{Deserialize, Serialize};

use super::{PtyOut, PtySession};
use crate::runner::processes::{Execution, Group};

const MAX_FRAME: usize = 1024 * 1024;
const DATA: u8 = 1;
const RESIZE: u8 = 2;
const EXIT: u8 = 3;
const ACK: u8 = 4;

#[derive(Serialize, Deserialize)]
struct Config {
    shell: Option<String>, command: Option<String>, program: Option<Vec<String>>,
    cwd: Option<std::path::PathBuf>, cols: u16, rows: u16, env: Vec<(String, String)>,
}

fn frame(kind: u8, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "CI terminal frame exceeds its limit");
    let mut out = Vec::with_capacity(bytes.len() + 5);
    out.push(kind);
    out.extend(u32::try_from(bytes.len())?.to_be_bytes());
    out.extend(bytes);
    Ok(out)
}

fn read_frame(input: &mut impl std::io::Read) -> anyhow::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    input.read_exact(&mut header)?;
    let (kind, length) = header.split_first().context("missing terminal frame header")?;
    let length = usize::try_from(u32::from_be_bytes(length.try_into()?))?;
    anyhow::ensure!(length <= MAX_FRAME, "CI terminal frame exceeds its limit");
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok((*kind, bytes))
}

/// Incremental framing for arbitrary pipe chunk boundaries; allocation is bounded before payload.
#[derive(Default)]
struct Decoder(Vec<u8>);
impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(self.0.len().saturating_add(bytes.len()) <= MAX_FRAME + 5 + 64 * 1024, "CI terminal pipe buffer exceeds its limit");
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    fn next(&mut self) -> anyhow::Result<Option<(u8, Vec<u8>)>> {
        let Some(header) = self.0.get(..5) else { return Ok(None) };
        let (kind, length) = header.split_first().context("missing terminal frame header")?;
        let kind = *kind;
        let length = usize::try_from(u32::from_be_bytes(length.try_into()?))?;
        anyhow::ensure!(length <= MAX_FRAME, "CI terminal frame exceeds its limit");
        let Some(bytes) = self.0.get(5..5 + length) else { return Ok(None) };
        let bytes = bytes.to_vec();
        drop(self.0.drain(..5 + length));
        Ok(Some((kind, bytes)))
    }
}

pub(super) struct Bridge {
    control: async_channel::Sender<Vec<u8>>,
    group: Arc<Group>,
    closed: crate::rt::Shutdown,
}
impl Bridge {
    pub(super) fn stop(&self) { self.closed.cancel(); self.group.kill(); }
    pub(super) fn resize(&self, cols: u16, rows: u16) {
        let bytes = [cols.clamp(2, 1000).to_be_bytes(), rows.clamp(2, 1000).to_be_bytes()].concat();
        if let Ok(bytes) = frame(RESIZE, &bytes) && self.control.try_send(bytes).is_err() { self.stop(); }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.closed.cancel();
        // U03: retire just this PTY group. The live job continues; its own completion also
        // owns this registered group and cannot release before the same kernel proof.
        self.group.kill();
    }
}

#[allow(clippy::too_many_arguments)] // Same launch parameters as the native terminal adapter.
pub(super) fn spawn(
    shell: Option<&str>, command: Option<&str>, program: Option<&[String]>,
    cwd: Option<&std::path::Path>, cols: u16, rows: u16, env: &[(&str, String)],
    execution: Arc<Execution>,
) -> anyhow::Result<(PtySession, async_channel::Receiver<PtyOut>)> {
    let config = Config {
        shell: shell.map(str::to_owned), command: command.map(str::to_owned), program: program.map(<[String]>::to_vec),
        cwd: cwd.map(std::path::Path::to_path_buf), cols, rows, env: env.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect(),
    };
    // Serialize and bound secrets before creating any child. They travel only through stdin.
    let config = frame(0, &serde_json::to_vec(&config)?)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--internal-ci-pty");
    // U03: the adapter needs one shard, independent of the runner's CPU count or runtime
    // overrides. Keep its task arena small as well; these are helper process settings only.
    cmd.env("KILN_RT_SHARDS", "1").env("KILN_RT_TASKS", "256");
    let (duplex, group) = crate::runner::processes::spawn_pty_bridge(cmd, &execution)?;
    let closed = crate::rt::Shutdown::new();
    let (control, commands) = async_channel::bounded::<Vec<u8>>(32);
    let (ack, acknowledged) = async_channel::bounded::<()>(1);
    // Reserved before spawning the forwarder: configuration always precedes input and resize.
    control.try_send(config)?;
    let (input, input_rx) = async_channel::bounded::<Vec<u8>>(32);
    let (tx, rx) = async_channel::bounded(256);
    let bridge = Bridge { control: control.clone(), group: Arc::clone(&group), closed: closed.clone() };
    let session = PtySession { input, bridge: Some(bridge), master: None, killer: None, handle: None, pid: None };
    let stopping = closed.clone();
    let input_group = Arc::clone(&group);
    let stdin = duplex.stdin.clone();
    crate::rt::spawn(async move {
        loop {
            let bytes = futures::select_biased! {
                () = stopping.cancelled().fuse() => break,
                bytes = commands.recv().fuse() => match bytes { Ok(bytes) => bytes, Err(_) => break },
                bytes = input_rx.recv().fuse() => match bytes {
                    Ok(bytes) => match frame(DATA, &bytes) { Ok(bytes) => bytes, Err(_) => break },
                    Err(_) => break,
                },
            };
            if stdin.send(bytes).await.is_err() { break }
            // The shared process interface has an unbounded input queue. One frame in flight
            // until the helper accepts it bounds that queue without changing other callers.
            let accepted = futures::select_biased! {
                () = stopping.cancelled().fuse() => false,
                got = acknowledged.recv().fuse() => got.is_ok(),
            };
            if !accepted { break }
        }
        stopping.cancel();
        input_group.kill();
    });
    // Drain stderr without displaying it: portable-pty errors may include an environment or
    // command. The public terminal receives only its framed data and exit status.
    crate::rt::spawn(async move { while duplex.stderr.recv().await.is_ok() {} });
    crate::rt::spawn(async move {
        let mut decoder = Decoder::default();
        let mut code = None;
        let mut valid = true;
        while let Ok(bytes) = duplex.stdout.recv().await {
            if decoder.push(&bytes).is_err() { valid = false; break }
            loop {
                match decoder.next() {
                    Ok(Some((DATA, data))) => {
                        if !closed.is_cancelled() {
                            let delivered = futures::select_biased! {
                                () = closed.cancelled().fuse() => true,
                                sent = tx.send(PtyOut::Data(data)).fuse() => sent.is_ok(),
                            };
                            if !delivered { valid = false; break }
                        }
                    }
                    Ok(Some((ACK, bytes))) if bytes.is_empty() => { if ack.try_send(()).is_err() { valid = false; break } },
                    Ok(Some((EXIT, bytes))) => match serde_json::from_slice::<Option<i32>>(&bytes) {
                        Ok(status) => { code = status; group.kill(); },
                        Err(_) => { valid = false; break },
                    },
                    Ok(None) => break,
                    _ => { valid = false; break },
                }
            }
            if !valid { break }
        }
        valid &= decoder.0.is_empty();
        closed.cancel();
        // EOF, a malformed frame or a disappeared consumer all retire this exact group.
        // Holding the group through proof also covers a helper that fails before PTY spawn.
        group.stop().await;
        let _ = duplex.exit.await;
        let _ = tx.send(PtyOut::Exit(if valid { code } else { None })).await;
    });
    crate::metrics::global().terminal_sessions.add(1);
    Ok((session, rx))
}

/// U03: private early CLI entry. Its caller was placed in the physical domain by the existing
/// native launcher before any code here runs. This entry does not acquire ownership itself.
/// It deliberately returns only a status; errors and configuration never reach logs or argv.
pub fn helper_main() -> std::process::ExitCode {
    match helper() { Ok(()) => std::process::ExitCode::SUCCESS, Err(_) => std::process::ExitCode::FAILURE }
}

fn helper() -> anyhow::Result<()> {
    let (kind, bytes) = read_frame(&mut std::io::stdin().lock())?;
    anyhow::ensure!(kind == 0, "CI terminal configuration must be first");
    let config: Config = serde_json::from_slice(&bytes)?;
    let (input, incoming) = async_channel::bounded(32);
    std::thread::Builder::new().name("kiln-ci-pty-input".into()).spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Ok(frame) = read_frame(&mut stdin) {
            if input.send_blocking(frame).is_err() { break }
        }
    })?;
    crate::rt::configure(crate::rt::Profile { shards: 1, tasks_per_shard: 256, pool: (1, 1, 1) });
    crate::rt::block_on(serve(config, incoming, std::io::stdout()))
}

// The writer is Send and owns no StdoutLock across either receive or PTY input await.
// Stdout takes its short internal lock only during each synchronous frame write/flush.
async fn serve(config: Config, incoming: async_channel::Receiver<(u8, Vec<u8>)>, mut stdout: impl std::io::Write + Send) -> anyhow::Result<()> {
    let env = config.env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect::<Vec<_>>();
    // No scoped execution exists in this process. The OS already owns this helper and
    // every PTY child it creates. portable-pty 0.9's Windows CreateProcessW uses only
    // EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT, never BREAKAWAY_FROM_JOB.
    let (session, output) = super::spawn_native(config.shell.as_deref(), config.command.as_deref(), config.program.as_deref(), config.cwd.as_deref(), config.cols, config.rows, &env, true)?;
    stdout.write_all(&frame(ACK, &[])?)?;
    stdout.flush()?;
    loop {
        futures::select_biased! {
            got = output.recv().fuse() => match got? {
                PtyOut::Data(bytes) => { stdout.write_all(&frame(DATA, &bytes)?)?; stdout.flush()?; },
                PtyOut::Exit(code) => { stdout.write_all(&frame(EXIT, &serde_json::to_vec(&code)?)?)?; stdout.flush()?; break },
            },
            got = incoming.recv().fuse() => {
                let (kind, bytes) = match got { Ok(got) => got, Err(_) => break };
                match kind {
                    DATA => session.input.send(bytes).await?,
                    RESIZE if bytes.len() == 4 => {
                        let cols = u16::from_be_bytes(bytes.get(..2).context("missing terminal columns")?.try_into()?);
                        let rows = u16::from_be_bytes(bytes.get(2..).context("missing terminal rows")?.try_into()?);
                        session.resize(cols, rows);
                    }
                    _ => anyhow::bail!("invalid CI terminal input frame"),
                }
                stdout.write_all(&frame(ACK, &[])?)?;
                stdout.flush()?;
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    struct ConptyFixtureOutput {
        bytes: Vec<u8>,
        decoder: Decoder,
        input: async_channel::Sender<(u8, Vec<u8>)>,
        query_tail: u32,
        cursor_replies: usize,
    }

    #[cfg(windows)]
    impl std::io::Write for ConptyFixtureOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            self.decoder.push(bytes).map_err(std::io::Error::other)?;
            while let Some((kind, payload)) = self.decoder.next().map_err(std::io::Error::other)? {
                if kind != DATA { continue }
                for byte in payload {
                    self.query_tail = (self.query_tail << 8) | u32::from(byte);
                    if self.query_tail == u32::from_be_bytes(*b"\x1b[6n") {
                        // U03: portable-pty requests PSEUDOCONSOLE_INHERIT_CURSOR. Emulate
                        // the browser terminal's reply, including queries split across frames.
                        self.input.try_send((DATA, b"\x1b[1;1R".to_vec())).map_err(std::io::Error::other)?;
                        self.cursor_replies += 1;
                    }
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    /// W17-R01/U03: the helper preserves the existing terminal fixture's TTY, size and exit
    /// semantics while accepting framed resize/input and environment over the private pipe.
    #[test]
    fn u03_helper_preserves_terminal_tty_resize_input_environment_and_exit() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let mut config = Config {
                shell: None, command: None,
                program: None, cwd: None, cols: 80, rows: 24, env: vec![("PTY_FIXTURE".into(), "original".into())],
            };
            #[cfg(unix)]
            {
                config.shell = Some("/bin/sh".into());
                config.command = Some("read line; stty size; test -t 1 && printf 'TTY:%s:%s\\n' \"$line\" \"$PTY_FIXTURE\"; exit 3".into());
            }
            #[cfg(windows)]
            {
                let powershell = crate::os::which("pwsh").or_else(|| crate::os::which("powershell"))
                    .context("the native ConPTY fixture requires standard PowerShell")?;
                // Console APIs distinguish a real ConPTY console from redirected pipes. Read
                // first so the resize frame has been applied before querying its dimensions.
                config.program = Some(vec![powershell.to_string_lossy().into_owned(), "-NoLogo".into(), "-NoProfile".into(), "-Command".into(),
                    "$line = [Console]::ReadLine(); if ([Console]::IsInputRedirected -or [Console]::IsOutputRedirected) { exit 4 }; [Console]::WriteLine(('{0} {1}' -f [Console]::WindowHeight, [Console]::WindowWidth)); [Console]::WriteLine(('TTY:{0}:{1}' -f $line, $env:PTY_FIXTURE)); exit 3".into()]);
            }
            let (input, incoming) = async_channel::bounded(4);
            input.send((RESIZE, vec![0, 100, 0, 30])).await?;
            input.send((DATA, if cfg!(windows) { b"fixture\r".to_vec() } else { b"fixture\n".to_vec() })).await?;
            #[cfg(not(windows))]
            let mut bytes = Vec::new();
            #[cfg(windows)]
            let mut bytes = ConptyFixtureOutput { bytes: Vec::new(), decoder: Decoder::default(), input: input.clone(), query_tail: 0, cursor_replies: 0 };
            crate::rt::timeout(std::time::Duration::from_secs(5), serve(config, incoming, &mut bytes)).await
                .map_err(|_| anyhow::anyhow!("CI PTY helper fixture timed out"))??;
            drop(input);
            #[cfg(not(windows))]
            let cursor_replies = 0;
            #[cfg(windows)]
            let (bytes, cursor_replies) = (bytes.bytes, bytes.cursor_replies);
            let mut framed = bytes.as_slice();
            let mut output = Vec::new();
            let mut code = None;
            let mut acknowledgements = 0;
            while !framed.is_empty() {
                let (kind, bytes) = read_frame(&mut framed)?;
                match kind {
                    DATA => output.extend(bytes),
                    ACK => acknowledgements += 1,
                    EXIT => code = serde_json::from_slice::<Option<i32>>(&bytes)?,
                    _ => anyhow::bail!("unexpected helper fixture frame"),
                }
            }
            let text = String::from_utf8_lossy(&output);
            assert!(text.contains("30 100"), "{text}");
            assert!(text.contains("TTY:fixture:original"), "{text}");
            assert_eq!(code, Some(3));
            assert_eq!(acknowledgements, 3 + cursor_replies);
            Ok(())
        })
    }

    #[test]
    fn u03_pty_frames_bound_payload_and_preserve_split_input_resize_and_exit() -> anyhow::Result<()> {
        let frames = [frame(DATA, b"echo TTY\r")?, frame(RESIZE, &[0, 100, 0, 30])?, frame(EXIT, b"0")?].concat();
        let mut decoder = Decoder::default();
        let mut decoded = Vec::new();
        for byte in frames { decoder.push(&[byte])?; if let Some(frame) = decoder.next()? { decoded.push(frame); } }
        assert_eq!(decoded, vec![(DATA, b"echo TTY\r".to_vec()), (RESIZE, vec![0, 100, 0, 30]), (EXIT, b"0".to_vec())]);
        decoder.push(&[DATA, 0xff, 0xff, 0xff, 0xff])?;
        assert!(decoder.next().is_err());
        assert!(read_frame(&mut &b"\x01\xff\xff\xff\xff"[..]).is_err());
        Ok(())
    }
}
