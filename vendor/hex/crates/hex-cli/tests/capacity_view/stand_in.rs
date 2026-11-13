//! The stand-in holder (joint plan step 13; `docs/design/capacity-view.md`, "What kiln's runner implements as a holder
//! on `CAPACITY_PORT`"): kiln's runner's side of the lane until kiln's own exists, `hex_api::capacity::Holder` driven
//! over `hex_quic::capacity::Link` as the contract has it, item by item:
//! 1. its identity is an enrollment made where it runs (its folder's `credentials`, `hex_quic::enroll::store`);
//! 2. it dials the keeper by the scheduler's name, asserts `TLS13_AES_256_GCM_SHA384`, and opens one link;
//! 3. the link frames each word (`hex_quic::capacity::Link`);
//! 4. hello, then its whole live set on the welcome, the counts compared on the synced; a run starts on its grant
//!    alone, is written to its ledger first, and its release is said once the run can no longer run;
//! 5. each answer is acted on as the contract says; a close ends the link, and the next word is a hello;
//! 6. down, it admits nothing;
//! 7. gone for good is the person's (`hex schedule forget`).
//!
//! Its ledger (`state.json` in its folder, written whole: a temporary file renamed over it) keeps its epoch, one more at
//! every start, written before it says anything, and the runs whose work may still run, so a start after a crash lists
//! them. Its runs do no work: each holds its reservation until the workload ends it.
//!
//! What it checks on every word: the CPU the view says is held never past the view's room (a machine's CPUs are its
//! hardware threads, so held CPU past them is an oversubscription; memory and disk are measured and may shrink below
//! what is held, which the view says as over), and every grant's view within its room in every dimension.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use hex_api::capacity::{
    Amounts, Event, Held, Holder, Out, ReservationId, ToHolder, ToKeeper, View, Why,
};
use hex_core::patience::Patience;
use hex_quic::capacity::Link;
use hex_quic::enroll::store;
use hex_quic::{CipherSuite, Config, Endpoint, PeerId, Refused, Shape};
use hex_testkit::waits::follow::{self, Party};

// The keeper's next word is waited for measuring, following the keeper by its life alone (decision 9): past the slowest
// answer seen the wait goes on, and ends with the word or the link's end, which the transport tells once its idle
// timeout passes (`hex_quic::limits`, three probe timeouts). A dial and a word said wait for their answers with no
// deadline of the test's: the driver ends a dial nothing answers at that timeout too (RFC 9000 §10.1), and a word cut
// at a slice's end would leave half a message on the link.
/// The keeper's next word: an answer, or a hint.
static HEAR: Patience = Patience::measuring("test.hex_cli.capacity_view.hear");

/// The link went down under a word: why, as the transport said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost(pub Refused);

/// What a reserve came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Granted,
    Refused(Why),
}

/// What the stand-in did and heard, for its runs' records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub granted: u64,
    pub refused_room: u64,
    pub refused_other: u64,
    pub unanswered: u64,
    pub released: u64,
    pub synced: u64,
    pub hints: u64,
    pub views: u64,
    pub links_lost: u64,
}

/// The stand-in holder (module docs).
pub struct StandIn {
    dir: PathBuf,
    name: String,
    keeper: String,
    address: SocketAddr,
    end: Option<Endpoint>,
    peer: Option<PeerId>,
    link: Option<Link>,
    holder: Holder,
    epoch: u64,
    runs: BTreeMap<ReservationId, Amounts>,
    pub tally: Tally,
}

/// `dir`'s ledger.
fn ledger(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

/// The stand-in's credentials, in its folder.
pub fn credentials(dir: &Path) -> PathBuf {
    dir.join("credentials")
}

impl StandIn {
    /// The stand-in named `name` (its certificate's name) whose folder is `dir`, its keeper the scheduler named `keeper`
    /// at `address`: its epoch one more than its ledger's, written before anything is said (item 2), and the runs its
    /// ledger lists, which may still run (item 3).
    pub fn start(dir: &Path, name: &str, keeper: &str, address: SocketAddr) -> Self {
        // The ledger's text: its epoch and its runs, as JSON.
        let (last, kept): (u64, Vec<Held>) = match std::fs::read(ledger(dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).expect("the stand-in's ledger"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (0, Vec::new()),
            Err(e) => panic!("reading the stand-in's ledger: {e}"),
        };
        let epoch = last + 1;
        let runs: BTreeMap<_, _> = kept.iter().map(|h| (h.id, h.amount)).collect();
        // A sync lists all a holder holds, and one sync fits one message (CV9): the most it may hold.
        let bound = hex_core::capacity::most_held().unwrap();
        let holder = Holder::start(name.to_owned(), epoch, bound, &kept).unwrap();
        let stand_in = Self {
            dir: dir.to_owned(),
            name: name.to_owned(),
            keeper: keeper.to_owned(),
            address,
            end: None,
            peer: None,
            link: None,
            holder,
            epoch,
            runs,
            tally: Tally::default(),
        };
        stand_in.keep();
        stand_in
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The runs whose work may run: its ledger's, as its sync lists them.
    pub fn held(&self) -> Vec<Held> {
        self.runs
            .iter()
            .map(|(id, amount)| Held {
                id: *id,
                amount: *amount,
            })
            .collect()
    }

    /// The view the keeper said last.
    pub fn view(&self) -> Option<View> {
        self.holder.view()
    }

    pub fn is_up(&self) -> bool {
        self.holder.is_up() && self.link.is_some()
    }

    /// The keeper is at `address` now (a scheduler started again on another port).
    pub fn moved(&mut self, address: SocketAddr) {
        self.address = address;
    }

    /// Its ledger written whole before anything said depends on it.
    fn keep(&self) {
        let text = serde_json::to_vec(&(self.epoch, self.held())).unwrap();
        let temp = self.dir.join("state.json.tmp");
        let mut file = std::fs::File::create(&temp).unwrap();
        file.write_all(&text).unwrap();
        file.sync_all().unwrap();
        drop(file);
        std::fs::rename(&temp, ledger(&self.dir)).unwrap();
    }

    /// Dials the keeper and opens a link (items 1 and 2): the connection's suite asserted AES-256-GCM.
    fn open(&mut self) -> Result<(), Lost> {
        if self.end.is_none() {
            self.end = Some(endpoint(&credentials(&self.dir), self.address));
        }
        let end = self.end.as_ref().unwrap();
        let peer = match self.peer {
            Some(peer) => peer,
            None => {
                let peer = end.dial(&self.keeper, self.address, None).map_err(Lost)?;
                let (suite, _) = end.negotiated(peer).map_err(Lost)?.unwrap();
                assert_eq!(suite, CipherSuite::TLS13_AES_256_GCM_SHA384);
                self.peer = Some(peer);
                peer
            }
        };
        self.link = Some(Link::open(end, peer).map_err(Lost)?);
        Ok(())
    }

    /// The link went down: what was asked is abandoned, never started (item 4), and the next word is a hello; the
    /// connection goes with it, so the next link is a new connection's (CV9).
    fn lose(&mut self, why: Refused) -> Lost {
        let mut out = Out::default();
        self.holder.link_down(&mut out);
        let unanswered = out
            .events
            .iter()
            .filter(|e| matches!(e, Event::Unanswered(_)))
            .count();
        self.tally.unanswered += u64::try_from(unanswered).unwrap();
        self.link = None;
        self.peer = None;
        self.end = None;
        self.tally.links_lost += 1;
        Lost(why)
    }

    /// The link drops at the stand-in's end, as a path that changed drops it: the keeper hears its connection end.
    pub fn drop_link(&mut self) {
        let mut out = Out::default();
        self.holder.link_down(&mut out);
        self.link = None;
        self.tally.links_lost += 1;
    }

    fn say(&mut self, msg: &ToKeeper) -> Result<(), Lost> {
        let said = self.link.as_ref().unwrap().say(msg, None);
        said.map_err(|why| self.lose(why))
    }

    /// The keeper's next word, checked and given to the core; what the core says back is said in turn. The events.
    fn hear_one(&mut self) -> Result<(ToHolder, Vec<Event>), Lost> {
        let link = self.link.as_mut().unwrap();
        let heard = follow::within(
            &HEAR,
            "the keeper's next word",
            || Party::Live,
            |by| match link.hear(by) {
                Err(Refused::Deadline { .. }) => None,
                heard => Some(heard),
            },
        )
        .unwrap();
        let msg = heard.map_err(|why| self.lose(why))?;
        if let Some(view) = view_of(&msg) {
            self.tally.views += 1;
            assert!(
                view.held.cpu_milli <= view.room.cpu_milli,
                "{}: the keeper holds CPU past the machine's: {msg:?}",
                self.name
            );
            if matches!(msg, ToHolder::Granted { .. }) {
                assert!(
                    view.held.fits(view.room),
                    "{}: a grant past the room: {msg:?}",
                    self.name
                );
            }
        }
        if matches!(msg, ToHolder::Hint { .. }) {
            self.tally.hints += 1;
        }
        let mut out = Out::default();
        self.holder.hear(msg.clone(), &mut out);
        for event in &out.events {
            // A keeper that serves this holder closes none of its sessions in these runs: a close is a defect here.
            if let Event::Closed(why) = event {
                panic!("{}: the keeper closed its session: {why:?}", self.name);
            }
        }
        for word in &out.send {
            self.say(word)?;
        }
        Ok((msg, out.events))
    }

    /// Opens a session (items 2 and 4): hello, its live set on the welcome, and the synced's counts compared with its
    /// own. Whether they agreed.
    pub fn join(&mut self) -> Result<bool, Lost> {
        if self.link.is_none() {
            self.open()?;
        }
        let hello = self.holder.connect().unwrap();
        self.say(&hello)?;
        loop {
            let (_, events) = self.hear_one()?;
            for event in events {
                if let Event::Synced { agree, .. } = event {
                    self.tally.synced += 1;
                    return Ok(agree);
                }
            }
        }
    }

    /// Asks for `amount`; on its grant the run is written to the ledger before its work would start (item 4).
    pub fn reserve(&mut self, amount: Amounts) -> Result<Asked, Lost> {
        let (id, msg) = self.holder.reserve(amount).unwrap();
        self.say(&msg)?;
        loop {
            let (_, events) = self.hear_one()?;
            for event in events {
                match event {
                    Event::Granted(granted) if granted == id => {
                        self.runs.insert(id, amount);
                        self.keep();
                        self.tally.granted += 1;
                        return Ok(Asked::Granted);
                    }
                    Event::Refused(refused, why) if refused == id => {
                        if why == Why::Room {
                            self.tally.refused_room += 1;
                        } else {
                            self.tally.refused_other += 1;
                        }
                        return Ok(Asked::Refused(why));
                    }
                    _ => {}
                }
            }
        }
    }

    /// `id`'s run can no longer run: out of the ledger first, then its release said (item 4).
    pub fn release(&mut self, id: ReservationId) -> Result<(), Lost> {
        self.runs.remove(&id);
        self.keep();
        let mut out = Out::default();
        self.holder.release(id, &mut out);
        for word in &out.send {
            self.say(word)?;
        }
        if out.send.is_empty() {
            return Ok(());
        }
        loop {
            let (_, events) = self.hear_one()?;
            if events
                .iter()
                .any(|e| matches!(e, Event::Released(released) if *released == id))
            {
                self.tally.released += 1;
                return Ok(());
            }
        }
    }

    /// Hears the keeper until the link goes, every word checked: how the transport said it went.
    pub fn until_lost(&mut self) -> Lost {
        loop {
            if let Err(lost) = self.hear_one() {
                return lost;
            }
        }
    }
}

/// An endpoint of the enrollment in `credentials`, bound on any address of `toward`'s family: one connection to the
/// keeper and the one replacing it, one lane on it (CV9, hex-quic.md Q8).
pub fn endpoint(credentials: &Path, toward: SocketAddr) -> Endpoint {
    let any: IpAddr = if toward.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    Endpoint::start(Config {
        credentials: store::load(credentials).unwrap(),
        bind: SocketAddr::new(any, 0),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 2,
        },
    })
    .unwrap()
}

fn view_of(msg: &ToHolder) -> Option<View> {
    match msg {
        ToHolder::Welcome { view, .. }
        | ToHolder::Synced { view, .. }
        | ToHolder::Granted { view, .. }
        | ToHolder::Refused { view, .. }
        | ToHolder::Released { view, .. }
        | ToHolder::Hint { view, .. } => Some(*view),
        ToHolder::Closed { .. } => None,
    }
}

/// A run's ask, shaped like a kiln run's (a leg's `needs: cpus=N,mem=M`): CPU and memory together, never a GPU, which
/// a holder can't reserve by count (CV3); one `share`-th of the machine's CPUs, and of its memory a quarter of that, so
/// a holder meets the room's edge within a few runs on a machine of any size. The shares are a test generator's.
pub fn run_of(room: Amounts, share: u64) -> Amounts {
    Amounts {
        cpu_milli: room.cpu_milli / share,
        mem_bytes: (room.mem_bytes / share / 4).max(hex_api::MIN_MEMORY),
        disk_bytes: 0,
        gpus: 0,
    }
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014), as hex-sim's: one seed names a workload.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// In `low..=high`.
    pub fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

/// `steps` steps of kiln-shaped work: with the runs it holds below the room's edge, it asks for one run more (a share
/// of 2 to 8 of the machine), else it ends one of its runs; every answer is checked as it comes (module docs). Ends at
/// its count, or when the link goes.
pub fn workload(stand_in: &mut StandIn, rng: &mut Rng, steps: u64) -> Result<(), Lost> {
    for _ in 0..steps {
        let view = stand_in
            .view()
            .expect("a synced stand-in has the keeper's view");
        let ask = run_of(view.room, rng.range(2, 8));
        let held = stand_in.held();
        let end = !held.is_empty() && (rng.range(0, 2) == 0 || !ask.fits(view.free()));
        if end {
            let last = u64::try_from(held.len()).unwrap() - 1;
            let at = usize::try_from(rng.range(0, last)).unwrap();
            stand_in.release(held[at].id)?;
        } else {
            stand_in.reserve(ask)?;
        }
    }
    Ok(())
}
