//! The Windows `Os`'s watch on a host's own connection (native plan step 8: [`Os::watch_source`], by which a
//! guest's control connection joins the core's loop on Windows), on a loopback connection: reported while what
//! it is watched for holds and until the core watches it again, as the core's level-triggered event set is;
//! reported no more once unwatched; and, watched for room while its sends wait, reported only once it has room,
//! so the core is never told a full connection can take more.

use super::*;

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::io::AsSocket;

use super::windows::Windows;

/// Longest a test waits for a fact it needs: a bound, so an `Os` that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(30);

/// A node's Windows `Os` on a socket of its own, in a folder that goes with it.
fn os() -> (tempfile::TempDir, Windows) {
    let dir = tempfile::Builder::new().prefix("ws-").tempdir().unwrap();
    let os = Windows::bind(&dir.path().join("s")).unwrap();
    (dir, os)
}

/// A loopback connection: the source's end, which never waits (as a source's reads and writes don't), and its
/// peer's.
fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    source.set_nonblocking(true).unwrap();
    (source, peer)
}

/// Whether a wait of at most `patience` reports `token`.
fn reported(os: &Windows, token: u64, patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    let mut events = Vec::new();
    loop {
        events.clear();
        os.wait(
            &mut events,
            Some(deadline.saturating_duration_since(Instant::now())),
        )
        .unwrap();
        if events
            .iter()
            .any(|e| matches!(e, Event::Ready { token: t, .. } if *t == token))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// Native plan step 8, a guest's connection in the core's loop on Windows: do watch a quiet source for
/// reading, send it a byte, wait twice, watch it again while the byte waits, read it and watch it again; then
/// unwatch it and send it another. Expect it unreported while quiet; reported once the byte comes and at the
/// next wait too (level-triggered, until the core watches it again); reported again when watched again while
/// the byte waits; quiet once read; and unwatched, never reported, its socket its owner's again (blocking
/// once more, which Winsock refuses a socket still watched). Then do watch it under a new token and end its
/// peer's side: expect the end reported, and again when watched again, as it stays ended.
#[test]
fn a_watched_source_is_reported_while_its_bytes_wait_or_it_has_ended_and_never_once_unwatched() {
    let (_dir, os) = os();
    let (mut source, mut peer) = pair();
    let token = 7;
    os.watch_source(source.as_socket(), token, false).unwrap();
    assert!(
        !reported(&os, token, Duration::ZERO),
        "a quiet source was reported"
    );
    peer.write_all(b"x").unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "a source a byte came to wasn't reported"
    );
    assert!(
        reported(&os, token, Duration::ZERO),
        "a source reported once wasn't reported at the next wait, before the core watched it again"
    );
    os.watch_source(source.as_socket(), token, false).unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "a source watched again while its byte waits wasn't reported"
    );
    assert_eq!(source.read(&mut [0u8; 8]).unwrap(), 1);
    os.watch_source(source.as_socket(), token, false).unwrap();
    assert!(
        !reported(&os, token, Duration::ZERO),
        "a source read to quiet and watched again was reported"
    );
    os.unwatch_source(source.as_socket(), token);
    source.set_nonblocking(false).unwrap();
    peer.write_all(b"y").unwrap();
    assert_eq!(source.read(&mut [0u8; 8]).unwrap(), 1);
    assert!(
        !reported(&os, token, Duration::ZERO),
        "an unwatched source was reported"
    );
    source.set_nonblocking(true).unwrap();
    let token = 8;
    os.watch_source(source.as_socket(), token, false).unwrap();
    peer.shutdown(Shutdown::Write).unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "a source whose peer ended wasn't reported"
    );
    os.watch_source(source.as_socket(), token, false).unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "an ended source watched again wasn't reported"
    );
    os.unwatch_source(source.as_socket(), token);
}

/// Native plan step 8, CLAUDE.md §3: do watch a long-running program and end it, then wait with the wait's
/// events already at their bound, so the one completion that wait takes from the port is the program's end;
/// then wait again. Expect the end reported: its completion is its only report, so a wait with no room for it
/// keeps it for the next, never drops it.
#[test]
fn a_programs_end_taken_by_a_full_wait_is_reported_by_the_next() {
    let (_dir, os) = os();
    let mut child = std::process::Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let token = 7;
    let Ok(watch) = os.watch_program(child.id(), token) else {
        panic!("the program wasn't watched");
    };
    child.kill().unwrap();
    child.wait().unwrap();
    let full = Event::Ready {
        token: 1000,
        readable: true,
        writable: false,
        hung_up: false,
    };
    let mut events = vec![full; EVENTS];
    os.wait(&mut events, Some(PATIENCE)).unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "a program's end taken by a wait with no room for it was never reported"
    );
    os.unwatch_program(watch);
}

/// Native plan step 8: do watch a source for room, then write to it until it takes no more and watch it for
/// room again, each time, until a wait finds it full and unreported; then let its peer read all of it. Expect
/// a source with room reported at once; every report while it fills finding room for a write (a full
/// connection reported for room would have the core write, find no room and ask again without end); and,
/// once its peer has read, room reported.
#[test]
fn a_source_watched_for_room_is_reported_only_while_it_has_room() {
    let (_dir, os) = os();
    let (mut source, mut peer) = pair();
    let token = 7;
    os.watch_source(source.as_socket(), token, true).unwrap();
    assert!(
        reported(&os, token, PATIENCE),
        "a source with room, watched for room, wasn't reported"
    );
    let chunk = vec![0u8; 64 << 10];
    let mut written = 0usize;
    let deadline = Instant::now() + PATIENCE;
    loop {
        assert!(Instant::now() < deadline, "the source never filled");
        match source.write(&chunk) {
            Ok(n) => {
                written += n;
                continue;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => panic!("writing the source: {e}"),
        }
        os.watch_source(source.as_socket(), token, true).unwrap();
        if !reported(&os, token, Duration::ZERO) {
            break;
        }
        match source.write(&chunk) {
            Ok(n) => written += n,
            Err(e) => panic!("a source reported for room had none: {e}"),
        }
    }
    let mut buf = vec![0u8; 64 << 10];
    let mut read = 0usize;
    while read < written {
        read += peer.read(&mut buf).unwrap();
    }
    assert!(
        reported(&os, token, PATIENCE),
        "a full source whose peer read all of it wasn't reported for room"
    );
    assert!(source.write(b"x").is_ok());
    os.unwatch_source(source.as_socket(), token);
}
