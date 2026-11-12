//! The box helper daemon's membership listing, rootless (HEX-MAC-13). `proc_listpids(PROC_UID_ONLY)` finds
//! this process's own children by its own user id — the mechanism the helper uses on a box's pool user,
//! which a daemonizing member can't escape. `kill_user` is not exercised here (it would signal this test's
//! whole user); it is measured as root on a dedicated pool user by `bench/native/macos/boxlab.sh`.

use std::collections::BTreeSet;
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use hex_api::{BoxId, Exit, Proc, Program};
use hex_core::helper::{START_FDS, VERSION};
use hex_core::pool::{Account, ROLE_UIDS, pool_index};
use rustix::net::{AddressFamily, SocketFlags, SocketType};

use super::super::helper_client::Helper;
use super::*;

const MIB: u64 = 1 << 20;

fn m(pid: i32, footprint: u64) -> Member {
    Member { pid, footprint }
}

/// HEX-MAC-14: a fresh box arms each member at its apportioned share; a read that changes nothing re-arms
/// nothing; a member growing shifts shares; a member gone is forgotten; a tree at the bound ends the box.
#[test]
fn reapportion_arms_the_changed_and_ends_when_over() {
    // 256 MiB bound, 16 MiB reserve → 240 MiB apportioned.
    let mut t = Tracked::new(7000, 256 * MIB, 16 * MIB, 64);

    // Two idle members: both armed at 120 MiB (240 / 2).
    let plan = t.reapportion(&[m(10, 0), m(11, 0)]);
    assert_eq!(plan, Plan::Arm(vec![(10, 120 * MIB), (11, 120 * MIB)]));

    // Same footprints again: nothing changed, so nothing to re-arm.
    assert_eq!(t.reapportion(&[m(10, 0), m(11, 0)]), Plan::Arm(vec![]));

    // One member now holds 100 MiB: headroom 140 MiB split as 70 each → 10's share 170, 11's 70. Both change.
    let plan = t.reapportion(&[m(10, 100 * MIB), m(11, 0)]);
    assert_eq!(plan, Plan::Arm(vec![(10, 170 * MIB), (11, 70 * MIB)]));

    // Member 11 gone: 10 alone gets all 240 MiB; 11 is forgotten.
    let plan = t.reapportion(&[m(10, 100 * MIB)]);
    assert_eq!(plan, Plan::Arm(vec![(10, 240 * MIB)]));
    assert!(!t.armed.contains_key(&11));

    // The tree reaches the apportionable bound: end the box, reporting the real bound.
    assert_eq!(
        t.reapportion(&[m(10, 140 * MIB), m(11, 100 * MIB)]),
        Plan::End {
            footprint: 240 * MIB,
            bound: 256 * MIB
        }
    );
}

/// An in-memory stand-in for the daemon's privileged backend: it tracks a box's members rather than
/// spawning or killing real processes, so the daemon's dispatch, apportionment and uid pool are driven
/// end-to-end over a real socket without root or side effects. The real backend is `helper_sys` (§12).
#[derive(Default)]
struct Sim {
    inner: Arc<Mutex<SimState>>,
    /// Signalled when a member event is posted (`fork`).
    posted: Arc<Condvar>,
}

#[derive(Default)]
struct SimState {
    next_pid: i32,
    members: Vec<Member>,
    arm_calls: u32,
    /// Programs whose end a watch read (the stand-in's reap).
    reaped: u32,
    /// The process cap each spawn was asked to set (`RLIMIT_NPROC` of the box's user), in order.
    nprocs: Vec<u32>,
    /// The directory's pool users (name, uid), which the daemon reads at start.
    pool_users: Vec<(String, u32)>,
    /// The ids held outside the pool (other users, groups, processes).
    taken: BTreeSet<u32>,
    /// The accounts the daemon asked to make, in order.
    made: Vec<Account>,
    /// The kept peak of concurrent boxes.
    peak: u32,
    /// Each pid armed, in order.
    armed: Vec<i32>,
    /// The members watched for their forks and ends.
    watched: BTreeSet<i32>,
    /// Member events posted and not yet taken, each the uid of the box its member is in.
    events: Vec<u32>,
    /// Each member's user (its box's uid).
    owner: BTreeMap<i32, u32>,
    /// How often each user's members were listed.
    listed: BTreeMap<u32, u32>,
    /// A member that forks between the daemon's listing and its watch, once (the race the re-list closes).
    fork_in_watch: Option<i32>,
}

/// The stand-in's fork: member `child` joins user `uid`'s box, and its parent's watch posts the event.
fn fork(state: &Arc<Mutex<SimState>>, posted: &Condvar, child: i32, uid: u32) {
    let mut s = state.lock().expect("sim lock");
    s.members.push(Member {
        pid: child,
        footprint: 0,
    });
    s.owner.insert(child, uid);
    s.events.push(uid);
    posted.notify_all();
}

/// The stand-in's watch: a program has ended (by KILL, the stand-in's only end) once it is no member.
struct SimProc {
    pid: i32,
    state: Arc<Mutex<SimState>>,
}

impl Proc for SimProc {
    fn pid(&self) -> u32 {
        u32::try_from(self.pid).unwrap_or(0)
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, hex_api::Refusal> {
        let mut s = self.state.lock().expect("sim lock");
        if s.members.iter().any(|m| m.pid == self.pid) {
            return Ok(None);
        }
        s.reaped += 1;
        Ok(Some(Exit::Signal(9)))
    }

    fn wait_for(&mut self, _timeout: Duration) -> Result<Option<Exit>, hex_api::Refusal> {
        self.try_wait()
    }

    /// The stand-in's KILL: the program is no member from then on.
    fn kill(&mut self) -> Result<(), hex_api::Refusal> {
        let mut s = self.state.lock().expect("sim lock");
        s.members.retain(|m| m.pid != self.pid);
        Ok(())
    }

    fn streams(&mut self) -> Result<hex_api::Given, hex_api::Refusal> {
        Ok(hex_api::Given::default())
    }
}

impl Privileged for Sim {
    fn spawn(
        &self,
        uid: u32,
        _program: &Program,
        fds: [OwnedFd; START_FDS],
        _leader: bool,
        _cap_mib: u32,
        nproc: u32,
    ) -> std::io::Result<i32> {
        drop(fds); // the stdio descriptors rode across; a real spawn would dup2 them.
        let mut s = self.inner.lock().expect("sim lock");
        s.nprocs.push(nproc);
        s.next_pid += 1;
        let pid = 1000 + s.next_pid;
        s.members.push(Member { pid, footprint: 0 });
        s.owner.insert(pid, uid);
        Ok(pid)
    }

    fn watch(&self, pid: i32) -> std::io::Result<Box<dyn Proc>> {
        Ok(Box::new(SimProc {
            pid,
            state: Arc::clone(&self.inner),
        }))
    }

    fn arm(&self, pid: i32, _cap_mib: u32) -> std::io::Result<()> {
        let mut s = self.inner.lock().expect("sim lock");
        s.arm_calls += 1;
        s.armed.push(pid);
        Ok(())
    }

    fn list(&self, uid: u32) -> std::io::Result<Vec<Member>> {
        let mut s = self.inner.lock().expect("sim lock");
        *s.listed.entry(uid).or_default() += 1;
        let owner = s.owner.clone();
        Ok(s.members
            .iter()
            .filter(|m| owner.get(&m.pid) == Some(&uid))
            .copied()
            .collect())
    }

    fn pool_users(&self) -> std::io::Result<Vec<(String, u32)>> {
        Ok(self.inner.lock().expect("sim lock").pool_users.clone())
    }

    fn taken_ids(&self) -> std::io::Result<BTreeSet<u32>> {
        let s = self.inner.lock().expect("sim lock");
        let mut taken = s.taken.clone();
        taken.extend(s.pool_users.iter().map(|(_, uid)| *uid));
        Ok(taken)
    }

    fn make_user(&self, account: &Account) -> std::io::Result<()> {
        let mut s = self.inner.lock().expect("sim lock");
        s.pool_users.push((account.name.clone(), account.uid));
        s.made.push(account.clone());
        Ok(())
    }

    fn peak(&self) -> std::io::Result<u32> {
        Ok(self.inner.lock().expect("sim lock").peak)
    }

    fn keep_peak(&self, peak: u32) -> std::io::Result<()> {
        self.inner.lock().expect("sim lock").peak = peak;
        Ok(())
    }

    fn watch_members(&self, pids: &[i32], uid: u32) -> std::io::Result<Vec<i32>> {
        let mut s = self.inner.lock().expect("sim lock");
        s.watched.extend(pids.iter().copied());
        // A fork that lands after the daemon listed the members and before this watch reached the child.
        if let Some(child) = s.fork_in_watch.take() {
            s.owner.insert(child, uid);
            s.members.push(Member {
                pid: child,
                footprint: 0,
            });
        }
        Ok(Vec::new())
    }

    fn next_member_event(&self) -> std::io::Result<Vec<u32>> {
        let mut s = self.inner.lock().expect("sim lock");
        while s.events.is_empty() {
            s = self.posted.wait(s).expect("sim lock");
        }
        Ok(std::mem::take(&mut s.events))
    }

    fn next_crossing(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the stand-in has no exception port",
        ))
    }

    fn kill(&self, uid: u32, signal: i32) -> std::io::Result<usize> {
        let mut s = self.inner.lock().expect("sim lock");
        let owner = s.owner.clone();
        let mine = |m: &Member| owner.get(&m.pid) == Some(&uid);
        let n = s.members.iter().filter(|m| mine(m)).count();
        // KILL (9) ends them; another signal is delivered but leaves them running (the stand-in's model).
        if signal == 9 {
            s.members.retain(|m| !mine(m));
        }
        Ok(n)
    }
}

fn prog() -> Program {
    Program {
        args: vec!["/bin/true".to_owned()],
        env: vec![],
        cwd: None,
        user: None,
    }
}

fn nulls() -> [OwnedFd; 3] {
    std::array::from_fn(|_| {
        OwnedFd::from(std::fs::File::open("/dev/null").expect("open /dev/null"))
    })
}

/// HEX-MAC-15 (the daemon end to end): over a real `SOCK_STREAM` socketpair, the node client's acquire,
/// start (stdio by SCM_RIGHTS), inspect, signal and end each drive the daemon's dispatch; the box takes a
/// pool user and gives it back on delete, each start is apportioned and armed, inspect counts the members,
/// and a deleted box is gone and its user reusable.
#[test]
fn the_daemon_serves_a_box_end_to_end() {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);

    // The daemon serves `server` on its own thread until the client closes.
    let handle = thread::spawn(move || {
        let daemon = Daemon::new(Sim::default(), vec![7000, 7001], 64);
        let _ = daemon.serve(server.as_fd());
        daemon
    });

    let client = Helper::over(client, VERSION);
    let b1 = BoxId::new("b1").expect("box id");

    // Acquire takes a pool user (the last pushed, 7001).
    assert_eq!(
        client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap(),
        7001
    );
    // A box already acquired is refused by name.
    assert!(matches!(
        client.acquire(&b1, 256 * MIB, 16 * MIB, None),
        Err(hex_api::Refusal::Exists(_))
    ));

    // Two members start; each is apportioned and armed; inspect counts them.
    let pid1 = client
        .start(&b1, &prog(), nulls().each_ref().map(|f| f.as_fd()), true)
        .unwrap();
    let pid2 = client
        .start(&b1, &prog(), nulls().each_ref().map(|f| f.as_fd()), false)
        .unwrap();
    assert!(pid1 > 0 && pid2 > 0 && pid1 != pid2);
    assert_eq!(client.inspect(&b1).unwrap().members, 2);

    // A non-KILL signal reaches the members and leaves them running.
    client.signal(&b1, 15).unwrap();
    assert_eq!(client.inspect(&b1).unwrap().members, 2);

    // Delete ends the box; it is then gone, and its user is back in the pool to be acquired again.
    client.end(&b1).unwrap();
    assert!(matches!(
        client.inspect(&b1),
        Err(hex_api::Refusal::NoSuchBox(_))
    ));
    assert_eq!(
        client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap(),
        7001
    );

    drop(client);
    let daemon = handle.join().expect("daemon thread");
    // Each start re-apportioned and armed at least once.
    assert!(daemon.ops.inner.lock().unwrap().arm_calls >= 2);
}

/// HEX-DIFF-1, HEX-DIFF-2 in box-user mode: once every member of a box has ended, a signal is refused as
/// not running (as every class refuses it), and the box still takes a program, which joins its user.
#[test]
fn a_box_with_no_live_member_refuses_a_signal_and_takes_a_program() {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);
    let handle = thread::spawn(move || {
        let daemon = Daemon::new(Sim::default(), vec![7000], 64);
        let _ = daemon.serve(server.as_fd());
    });

    let client = Helper::over(client, VERSION);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    client
        .start(&b1, &prog(), nulls().each_ref().map(|f| f.as_fd()), true)
        .unwrap();

    // KILL ends every member; the box stays, with none live.
    client.signal(&b1, 9).unwrap();
    assert_eq!(client.inspect(&b1).unwrap().members, 0);
    assert!(matches!(
        client.signal(&b1, 15),
        Err(hex_api::Refusal::NotRunning(_, "signal"))
    ));

    // The next program starts as the box's user, and a signal reaches it again.
    client
        .start(&b1, &prog(), nulls().each_ref().map(|f| f.as_fd()), false)
        .unwrap();
    assert_eq!(client.inspect(&b1).unwrap().members, 1);
    client.signal(&b1, 15).unwrap();

    drop(client);
    handle.join().expect("daemon thread");
}

/// A connected client to a daemon over the stand-in, with `pool` users under a per-uid process ceiling
/// `per_uid`, the daemon serving on its own thread until the client closes.
fn served(pool: Vec<u32>, per_uid: u32) -> (Helper, thread::JoinHandle<Daemon<Sim>>) {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);
    let handle = thread::spawn(move || {
        let daemon = Daemon::new(Sim::default(), pool, per_uid);
        let _ = daemon.serve(server.as_fd());
        daemon
    });
    (Helper::over(client, VERSION), handle)
}

fn start(client: &Helper, b: &BoxId, leader: bool) -> i32 {
    client
        .start(b, &prog(), nulls().each_ref().map(|f| f.as_fd()), leader)
        .unwrap()
}

/// HEX-MAC-15, HEX-RT-3 (box-user mode): do start two programs and end one; expect a wait on the running
/// one to answer it runs, a wait on the ended one its end, and a second wait on it Unknown (an end is
/// read once, so the helper holds no end the node has read).
#[test]
fn the_helper_reaps_a_program_and_answers_its_end_once() {
    let (client, handle) = served(vec![7000], 64);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    let pid1 = start(&client, &b1, true);
    assert_eq!(client.wait(&b1, pid1).unwrap(), None);
    client.signal(&b1, 9).unwrap();
    assert_eq!(client.wait(&b1, pid1).unwrap(), Some(Exit::Signal(9)));
    assert_eq!(client.wait(&b1, pid1).unwrap(), Some(Exit::Unknown));
    drop(client);
    let daemon = handle.join().expect("daemon thread");
    assert_eq!(daemon.ops.inner.lock().unwrap().reaped, 1);
}

/// HEX-RT-16 (box-user mode; the lead, 2026-11-09): of two programs the helper started in a box, the one the
/// node kills by its pid ends by the helper's hand, since the node may not signal the pool user, and the other
/// runs on; a second kill of the ended one, and a kill of a pid the box doesn't hold, signal nothing and are
/// done; a kill in a box the helper doesn't know is refused by name. Do start two programs, kill the first
/// twice and a pid the box never started once, then kill in an unknown box; expect the first's end Signal(9),
/// the second still running and a member, and the unknown box refused.
#[test]
fn a_box_user_program_is_killed_alone_through_the_helper() {
    let (client, handle) = served(vec![7000], 64);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    let pid1 = start(&client, &b1, true);
    let pid2 = start(&client, &b1, false);
    client.kill(&b1, pid1).unwrap();
    client.kill(&b1, pid1).unwrap();
    client.kill(&b1, pid2 + 1000).unwrap();
    assert_eq!(client.wait(&b1, pid1).unwrap(), Some(Exit::Signal(9)));
    assert_eq!(client.wait(&b1, pid2).unwrap(), None);
    let unknown = BoxId::new("nobox").expect("box id");
    assert!(
        matches!(client.kill(&unknown, pid2), Err(hex_api::Refusal::NoSuchBox(b)) if b == unknown),
        "a kill in an unknown box wasn't refused by name"
    );
    drop(client);
    let daemon = handle.join().expect("daemon thread");
    let s = daemon.ops.inner.lock().unwrap();
    assert!(
        s.members.iter().any(|m| m.pid == pid2),
        "the box's other program ended with the first"
    );
}

/// HEX-MAC-15, HEX-RT-3 (box-user mode): do delete a box with two programs running; expect both reaped
/// by the delete (no zombie left under its user) and each one's end still answered after it, as a
/// deleted box's program ends by the delete's kill on every class.
#[test]
fn a_deleted_boxs_programs_are_reaped_and_their_ends_kept() {
    let (client, handle) = served(vec![7000], 64);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    let pid1 = start(&client, &b1, true);
    let pid2 = start(&client, &b1, false);
    client.end(&b1).unwrap();
    assert_eq!(client.wait(&b1, pid1).unwrap(), Some(Exit::Signal(9)));
    assert_eq!(client.wait(&b1, pid2).unwrap(), Some(Exit::Signal(9)));
    drop(client);
    let daemon = handle.join().expect("daemon thread");
    assert_eq!(daemon.ops.inner.lock().unwrap().reaped, 2);
}

/// CLAUDE.md §2 (the helper's programs are bounded): do start `nproc` programs in a box whose spec names
/// `pids_max` 2;
/// expect a third refused by name while both run, and taken once they have ended (the oldest unread end
/// is dropped for it, and reads Unknown).
#[test]
fn a_box_holds_at_most_its_process_cap_of_programs() {
    let (client, handle) = served(vec![7000], 64);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, Some(2)).unwrap();
    let pid1 = start(&client, &b1, true);
    let _pid2 = start(&client, &b1, false);
    assert!(matches!(
        client.start(&b1, &prog(), nulls().each_ref().map(|f| f.as_fd()), false),
        Err(hex_api::Refusal::Os { .. })
    ));
    client.signal(&b1, 9).unwrap();
    let _pid3 = start(&client, &b1, false);
    assert_eq!(client.wait(&b1, pid1).unwrap(), Some(Exit::Unknown));
    drop(client);
    handle.join().expect("daemon thread");
}

/// A connected client, the daemon (shared so a test drives its pumps), and the stand-in's state and signal.
type Shared = (Helper, Arc<Daemon<Sim>>, Arc<Mutex<SimState>>, Arc<Condvar>);

/// A daemon over the stand-in, shared so a test drives its member pump beside the socket it serves, with
/// the stand-in's state and event signal kept for the test.
fn shared(nproc: u32) -> Shared {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);
    let sim = Sim::default();
    let (state, posted) = (Arc::clone(&sim.inner), Arc::clone(&sim.posted));
    let daemon = Arc::new(Daemon::new(sim, vec![7000, 7001], nproc));
    let serving = Arc::clone(&daemon);
    thread::spawn(move || {
        let _ = serving.serve(server.as_fd());
    });
    (Helper::over(client, VERSION), daemon, state, posted)
}

/// HEX-MAC-14, CLAUDE.md §5 (no poll period): do start a member, then fork it; expect the started member
/// watched for its forks, and the child armed and watched on the fork's event, the daemon waking on that
/// event alone, with no period to pick.
#[test]
fn a_forked_member_is_armed_on_its_fork_event() {
    let (client, daemon, state, posted) = shared(64);
    let b1 = BoxId::new("b1").expect("box id");
    let uid = client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    let leader = start(&client, &b1, true);
    assert!(state.lock().unwrap().watched.contains(&leader));
    let pumping = Arc::clone(&daemon);
    let turn = thread::spawn(move || pumping.pump_members());
    fork(&state, &posted, 4242, uid);
    turn.join().expect("pump thread").expect("one turn");
    let s = state.lock().unwrap();
    assert!(
        s.armed.contains(&4242),
        "the child wasn't armed: {:?}",
        s.armed
    );
    assert!(
        s.watched.contains(&4242),
        "the child isn't watched for its own forks"
    );
}

/// HEX-MAC-14, the lead's rule on skipped wins: do run two boxes and fork a member of one; expect the
/// sweep that fork wakes to list that box's user alone, the event naming its box (its watch's tag), so a
/// fork costs one box's listing however many boxes the node runs.
#[test]
fn a_fork_sweeps_only_its_own_box() {
    let (client, daemon, state, posted) = shared(64);
    let (b1, b2) = (BoxId::new("b1").expect("id"), BoxId::new("b2").expect("id"));
    let u1 = client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    let u2 = client.acquire(&b2, 256 * MIB, 16 * MIB, None).unwrap();
    start(&client, &b1, true);
    start(&client, &b2, true);
    let before = state.lock().unwrap().listed.clone();
    let pumping = Arc::clone(&daemon);
    let turn = thread::spawn(move || pumping.pump_members());
    fork(&state, &posted, 4343, u1);
    turn.join().expect("pump thread").expect("one turn");
    let s = state.lock().unwrap();
    let grew =
        |u: u32| s.listed.get(&u).copied().unwrap_or(0) - before.get(&u).copied().unwrap_or(0);
    assert!(grew(u1) > 0, "the forking box wasn't swept");
    assert_eq!(grew(u2), 0, "a box with no event was listed");
    assert!(s.armed.contains(&4343));
}

/// HEX-MAC-14 (the fork window's race): do fork a member after the daemon lists the box and before its
/// watch is placed; expect the daemon's re-list to find the child and watch it too, so no member is left
/// whose forks would post no event.
#[test]
fn a_member_forked_before_its_watch_is_caught_by_the_relist() {
    let (client, _daemon, state, _posted) = shared(64);
    let b1 = BoxId::new("b1").expect("box id");
    client.acquire(&b1, 256 * MIB, 16 * MIB, None).unwrap();
    state.lock().unwrap().fork_in_watch = Some(5151);
    start(&client, &b1, true);
    let s = state.lock().unwrap();
    assert!(
        s.watched.contains(&5151),
        "a member was left unwatched: {:?}",
        s.watched
    );
    assert!(s.armed.contains(&5151));
}

/// A few sleeping children of this process, killed when dropped.
struct Sleepers(Vec<Child>);

impl Drop for Sleepers {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// HEX-MAC-13: every live child this process started is found in its user's listing, by pid.
#[test]
fn members_lists_the_users_live_processes() {
    let uid = rustix::process::getuid().as_raw();
    let sleepers = Sleepers(
        (0..3)
            .filter_map(|_| Command::new("/bin/sleep").arg("30").spawn().ok())
            .collect(),
    );
    assert_eq!(sleepers.0.len(), 3, "spawned three sleepers");
    let want: Vec<i32> = sleepers
        .0
        .iter()
        .map(|c| i32::try_from(c.id()).expect("a pid"))
        .collect();

    // Wait on the fact, not the clock (CLAUDE.md §4): read the listing until every child is in it, guarded by
    // twice the slowest recorded run of the macOS tests that wait (39.507 s,
    // bench/native/macos/records/test-waits-macos.txt). The kernel knows a child at fork, so the first read
    // usually already has them.
    let mut pids = std::collections::BTreeSet::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(39_507);
    while std::time::Instant::now() < deadline {
        pids = members(uid)
            .expect("list the user's members")
            .iter()
            .map(|m| m.pid)
            .collect();
        if want.iter().all(|p| pids.contains(p)) {
            break;
        }
        std::hint::spin_loop();
    }
    for pid in &want {
        assert!(
            pids.contains(pid),
            "member {pid} not found in the user's listing"
        );
    }
    // This process itself is one of its user's processes.
    let me = rustix::process::getpid().as_raw_nonzero().get();
    assert!(pids.contains(&me), "the listing holds this process {me}");
}

/// Ada's rule (2026-11-06): no picked defaults. A box's process cap is its spec's `pids_max` when it names
/// one, else the platform's per-uid ceiling (`kern.maxprocperuid`, which the helper reads at start): each
/// box has a uid of its own, and XNU's fork counts a uid's processes against its `RLIMIT_NPROC`
/// (`bsd/kern/kern_fork.c:269-283`). Linux's class sets no default either (`pids.max` only from
/// `pids_max`). Do start a program in a box naming none and in one naming 5, under a ceiling of 64;
/// expect the spawns asked for 64 and 5.
#[test]
fn a_box_naming_no_process_limit_gets_the_per_uid_ceiling() {
    let (client, handle) = served(vec![7000, 7001], 64);
    let plain = BoxId::new("plain").expect("box id");
    let limited = BoxId::new("limited").expect("box id");
    client.acquire(&plain, 256 * MIB, 16 * MIB, None).unwrap();
    client
        .acquire(&limited, 256 * MIB, 16 * MIB, Some(5))
        .unwrap();
    start(&client, &plain, true);
    start(&client, &limited, true);
    drop(client);
    let daemon = handle.join().expect("daemon thread");
    assert_eq!(daemon.ops.inner.lock().unwrap().nprocs, vec![64, 5]);
}

/// A connected client to a daemon that grows its pool, over a stand-in whose directory holds `pool_users`,
/// with `taken` ids held outside it and a kept `peak`.
fn served_growing(
    pool_users: Vec<(String, u32)>,
    taken: BTreeSet<u32>,
    peak: u32,
) -> (
    Helper,
    Arc<Mutex<SimState>>,
    thread::JoinHandle<Daemon<Sim>>,
) {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);
    let sim = Sim::default();
    {
        let mut s = sim.inner.lock().unwrap();
        s.pool_users = pool_users;
        s.taken = taken;
        s.peak = peak;
    }
    let state = Arc::clone(&sim.inner);
    let daemon = Daemon::growing(sim, 64).expect("a growing daemon");
    let handle = thread::spawn(move || {
        let _ = daemon.serve(server.as_fd());
        daemon
    });
    (Helper::over(client, VERSION), state, handle)
}

/// The lead's #2 (2026-11-06): a hexnode member can't make the helper create anything outside Apple's role
/// range or the pool's name pattern, nor at an id someone holds. Do acquire 60 boxes (more than the range
/// holds) through the socket, with ids 455 and 9000 held outside the pool and one pool user at 450; expect
/// every account made to be `_hexbox<n>` in 450-499 and never 455, one made per box past the first, and
/// the acquires past the range refused by name.
#[test]
fn a_member_cannot_make_an_account_outside_the_range_or_the_pattern() {
    let (client, state, handle) = served_growing(
        vec![("_hexbox1".to_owned(), 450)],
        BTreeSet::from([455, 9000]),
        0,
    );
    let mut refused = 0;
    for i in 0..60 {
        let b = BoxId::new(&format!("hostile-{i}")).expect("box id");
        match client.acquire(&b, 256 * MIB, 16 * MIB, None) {
            Ok(uid) => assert!(ROLE_UIDS.contains(&uid) && uid != 455, "uid {uid}"),
            Err(hex_api::Refusal::Os { .. }) => refused += 1,
            Err(e) => panic!("{e}"),
        }
    }
    drop(client);
    handle.join().expect("daemon thread");
    let made = state.lock().unwrap().made.clone();
    // 50 role uids, less 450 (the pool's) and 455 (held): 48 accounts, then refusals.
    assert_eq!(made.len(), 48);
    assert_eq!(refused, 60 - 49);
    for a in &made {
        assert!(pool_index(&a.name).is_some(), "{}", a.name);
        assert!(ROLE_UIDS.contains(&a.uid) && a.uid != 455, "{}", a.uid);
    }
}

/// The lead's #2: an id is checked free just before its account is made, since another installer may have
/// taken it since install. Do take 451 after the daemon starts; expect the account made at 452.
#[test]
fn an_id_taken_after_start_is_skipped() {
    let (client, state, handle) =
        served_growing(vec![("_hexbox1".to_owned(), 450)], BTreeSet::new(), 0);
    state.lock().unwrap().taken.insert(451);
    let a = BoxId::new("a").expect("box id");
    let b = BoxId::new("b").expect("box id");
    client.acquire(&a, 256 * MIB, 16 * MIB, None).unwrap();
    assert_eq!(client.acquire(&b, 256 * MIB, 16 * MIB, None).unwrap(), 452);
    drop(client);
    handle.join().expect("daemon thread");
    assert_eq!(
        state.lock().unwrap().made,
        vec![Account {
            name: "_hexbox2".to_owned(),
            uid: 452
        }]
    );
}

/// The lead's #2: the pool stays ahead by the node's observed peak of concurrent box-user boxes, kept
/// across restarts. Do start a daemon with one pool user and a kept peak of 3, then hold 4 boxes at once;
/// expect 2 accounts made at start, a 4th when the 4th box comes, and the kept peak 4.
#[test]
fn the_pool_is_grown_to_the_kept_peak_and_the_peak_is_kept() {
    let (client, state, handle) =
        served_growing(vec![("_hexbox1".to_owned(), 450)], BTreeSet::new(), 3);
    assert_eq!(state.lock().unwrap().made.len(), 2);
    for i in 0..4 {
        let b = BoxId::new(&format!("b{i}")).expect("box id");
        client.acquire(&b, 256 * MIB, 16 * MIB, None).unwrap();
    }
    drop(client);
    handle.join().expect("daemon thread");
    let s = state.lock().unwrap();
    assert_eq!(s.made.len(), 3);
    assert_eq!(s.peak, 4);
}
