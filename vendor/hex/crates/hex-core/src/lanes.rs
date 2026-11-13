//! Hex's connections over hyper-transport's lanes (docs/design/hex-quic.md Q3): byte streams between two
//! peers, each to a port on the peer, carried by lanes that are used again once a connection lets them go.
//! Pure: the shell (`hex-quic`) queues the frames this says to send and hands it the frames that come;
//! this decides what each lane carries, when a lane is free again, what a frame means, and refuses by type
//! what a bound or the protocol doesn't allow (CLAUDE.md §2, §3, §9).
//!
//! A hyper-transport lane is a unidirectional stream that lasts as long as its connection (hyper-transport
//! `lane.rs`): frames in order, never closed. So a connection here holds one lane of each side's while it
//! is open, and lets it go at its half's end, for another connection to take:
//!
//! - **Open** `port, id` on the opener's lane binds that lane to a new connection to `port`;
//! - **Accept** `id` on the acceptor's lane binds that lane to it, or **Refuse** `id, why` on the
//!   acceptor's lane 0 says why it can't be (lane 0 is never bound, so a refusal always has a lane);
//! - **data** frames carry its bytes, of the kind its port's class travels in ([`data_kind`]);
//! - **End** ends the sender's half: its lane is free again from the next frame on;
//! - **Reset** `why` ends both halves at once: the other side stops sending, ends its half, and what it
//!   had sent before is let go, by the owner's own word;
//! - **Welcome**, on lane 0, is the accepting side's word that it admitted the peer: TLS 1.3 completes a client's
//!   handshake before the server has verified the client's certificate (RFC 8446 §4.4.4, RFC 9001 §4.1.2), so a
//!   dialer counts itself connected once it has heard it;
//! - **Credit** `bytes` on a side's lane lets the peer send that many more bytes of the connection: each side may
//!   send [`INITIAL_CREDIT`] at first, and its peer grants what its owner has read. A reader that stops holds its own
//!   connection alone: its peer stops at its credit, while QUIC's connection window, which every stream shares and
//!   only bytes read give back, keeps flowing, since every lane is read as frames come (docs/design/hex-quic.md Q3).
//!
//! A frame on a lane is read in order after the frames before it, so a lane taken again after an End
//! needs no more than its next Open or Accept to say what it carries now.

use std::collections::BTreeMap;

use crate::stream::{self, files};

/// A lane, as hyper-transport numbers them (`hyper_transport::LaneId`).
pub type LaneId = u32;

/// The lane every side keeps for its refusals, never bound to a connection: a peer's Open is answered
/// even when every other lane is taken.
pub const REFUSALS: LaneId = 0;

/// The node's log, on a connection of its own: the port after the guest's (crate::stream's ports, which
/// spell `HEX` so a capture reads them), so one namespace names every port a peer opens.
pub const LOG_PORT: u32 = 0x4845_5804;

/// The machine's capacity view (joint plan step 13, `docs/design/capacity-view.md` CV9): a holder's connection
/// to its keeper, the port after the log's. Its messages are a control's, small and answered one by one.
pub const CAPACITY_PORT: u32 = 0x4845_5805;

/// The most bytes one data frame carries: one whole framed wire message ([`stream::MAX_FRAME`]), the
/// longest unit any port's stream carries whole, so a control message never needs two frames.
pub const DATA_BOUND: usize = stream::MAX_FRAME;

/// The bytes of a connection each side may send before its peer grants more: two whole frames, one its peer's owner
/// reads while the next waits, so a reader that keeps up never waits on its peer's credit, and one that doesn't holds
/// at most these.
pub const INITIAL_CREDIT: u64 = 2 * DATA_BOUND as u64;

/// What a frame is: its kind's code on the wire is hyper-transport's (`Classes::kind_code`). Codes only
/// append; a peer that speaks another version of this protocol is refused at the handshake by its
/// application protocol (docs/design/hex-quic.md Q3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Open,
    Accept,
    Refuse,
    End,
    Reset,
    /// Bytes of a connection to the control port.
    ControlData,
    /// Bytes of a stdio stream.
    StreamData,
    /// Bytes of a staging or log connection.
    BulkData,
    Welcome,
    Credit,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Kind::Open,
        Kind::Accept,
        Kind::Refuse,
        Kind::End,
        Kind::Reset,
        Kind::ControlData,
        Kind::StreamData,
        Kind::BulkData,
        Kind::Welcome,
        Kind::Credit,
    ];

    pub const fn code(self) -> u16 {
        match self {
            Kind::Open => 1,
            Kind::Accept => 2,
            Kind::Refuse => 3,
            Kind::End => 4,
            Kind::Reset => 5,
            Kind::ControlData => 6,
            Kind::StreamData => 7,
            Kind::BulkData => 8,
            Kind::Welcome => 9,
            Kind::Credit => 10,
        }
    }

    pub fn of(code: u16) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.code() == code)
    }

    /// The class a frame of this kind travels in: a connection's own frames, which a peer waits on to go on,
    /// with the control port's bytes.
    pub const fn class(self) -> Class {
        match self {
            Kind::Open
            | Kind::Accept
            | Kind::Refuse
            | Kind::End
            | Kind::Reset
            | Kind::Welcome
            | Kind::Credit
            | Kind::ControlData => Class::Control,
            Kind::StreamData => Class::Stream,
            Kind::BulkData => Class::Bulk,
        }
    }
}

/// How urgent a frame is, most urgent first: hyper-transport keeps a reserve of a connection's credit for
/// each class above another (`credit::class_reserve`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// The node agent's and the scheduler's calls and answers, and every connection's own frames.
    Control,
    /// A program's stdin, stdout and stderr.
    Stream,
    /// File trees staged and the node's log.
    Bulk,
}

impl Class {
    /// How many classes there are (`Classes::RANKS`).
    pub const RANKS: u8 = 3;

    pub const fn rank(self) -> u8 {
        match self {
            Class::Control => 0,
            Class::Stream => 1,
            Class::Bulk => 2,
        }
    }
}

/// The data kind a port's bytes travel as, or `None` for a port no peer serves.
pub fn data_kind(port: u32) -> Option<Kind> {
    match port {
        stream::CONTROL_PORT | CAPACITY_PORT => Some(Kind::ControlData),
        stream::STDIO_PORT => Some(Kind::StreamData),
        files::FILES_PORT | LOG_PORT => Some(Kind::BulkData),
        _ => None,
    }
}

/// A connection: who opened it, and its id, the opener's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Conn {
    /// Opened by this side.
    pub here: bool,
    pub id: u64,
}

/// Why a connection was refused or reset, as its frame says it. Codes only append; one this side doesn't
/// know stays typed as [`Why::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Why {
    #[error("nothing serves that port there")]
    NoPort,
    #[error("every lane of the side asked is taken")]
    Lanes,
    #[error("the side asked holds its most connections")]
    Conns,
    #[error("its owner let it go")]
    Gone,
    #[error("a frame broke the lanes' protocol")]
    Protocol,
    /// Never on the wire: more bytes than the peer allows now ([`Mux::room`]).
    #[error("the peer allows no more of its bytes now")]
    Credit,
    /// Never on the wire: the connection to the peer closed, and every connection on it with it.
    #[error("the connection to the peer closed")]
    Closed,
    #[error("refused for a reason this side doesn't know (code {0})")]
    Other(u8),
}

impl Why {
    const fn code(self) -> u8 {
        match self {
            Why::NoPort => 1,
            Why::Lanes => 2,
            Why::Conns => 3,
            Why::Gone => 4,
            Why::Protocol => 5,
            Why::Closed => 6,
            Why::Credit => 7,
            Why::Other(code) => code,
        }
    }

    fn of(code: u8) -> Why {
        match code {
            1 => Why::NoPort,
            2 => Why::Lanes,
            3 => Why::Conns,
            4 => Why::Gone,
            5 => Why::Protocol,
            6 => Why::Closed,
            7 => Why::Credit,
            other => Why::Other(other),
        }
    }
}

/// A peer broke the protocol: the shell ends its connection, typed, naming what it sent.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the peer broke the lanes' protocol: {0}")]
pub struct Broken(pub String);

/// What the mux tells its shell to do, and its owner.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// Queue a frame of `kind` with `payload` on this side's lane `lane` to the peer, after the lane's earlier
    /// frames.
    Send {
        lane: LaneId,
        kind: Kind,
        payload: Vec<u8>,
    },
    /// The peer opened `conn` to `port`, served here: it is open both ways.
    Accepted { conn: Conn, port: u32 },
    /// The peer accepted `conn`, opened here.
    Opened { conn: Conn },
    /// `conn`, opened here, was refused: it is gone.
    Refused { conn: Conn, why: Why },
    /// Bytes of `conn` from the peer, in order.
    Data { conn: Conn, bytes: Vec<u8> },
    /// The peer's half of `conn` ended: no more bytes come.
    Ended { conn: Conn },
    /// `conn` was reset, by the peer or with the connection to it: it is gone both ways.
    Reset { conn: Conn, why: Why },
    /// Both halves of `conn` ended: it is gone, and its lanes are free again.
    Closed { conn: Conn },
    /// The peer admitted this side.
    Welcomed,
    /// The peer granted `conn` more room: what waited for it may go.
    Credit { conn: Conn },
}

/// The mux's bounds, each its owner's derived count (docs/design/hex-quic.md Q8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// This side's lanes to the peer, beside [`REFUSALS`]: at most this many connections hold one at once.
    pub lanes: u32,
}

/// One connection's two halves. Each holds its lane until the connection is gone (both halves ended, or reset,
/// or refused), so a reset after a half's End still travels on that half's lane, after its End.
#[derive(Debug)]
struct State {
    port: u32,
    data: Kind,
    here: Here,
    there: There,
    /// The bytes the peer allows this side to send in all, and those sent.
    allowed: u64,
    sent: u64,
    /// The bytes this side allows the peer to send in all, and those it sent.
    granted: u64,
    received: u64,
}

impl State {
    fn new(port: u32, data: Kind, here: Here, there: There) -> Self {
        Self {
            port,
            data,
            here,
            there,
            allowed: INITIAL_CREDIT,
            sent: 0,
            granted: INITIAL_CREDIT,
            received: 0,
        }
    }
}

/// This side's half, on this side's lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Here {
    Open(LaneId),
    Ended(LaneId),
}

impl Here {
    fn lane(self) -> LaneId {
        match self {
            Here::Open(lane) | Here::Ended(lane) => lane,
        }
    }
}

/// The peer's half: not yet named (an Open sent, no Accept yet), or on the peer's lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum There {
    Pending,
    On(LaneId),
    Ended(LaneId),
}

/// What one of the peer's lanes carries now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// A connection's frames.
    Bound(Conn),
    /// A connection's that is gone here (refused or reset): its frames are let go until the peer's End or Reset
    /// frees the lane.
    Discard,
}

/// One peer connection's lanes and the connections on them.
#[derive(Debug)]
pub struct Mux {
    limits: Limits,
    /// The ports this side serves.
    serves: Vec<u32>,
    /// This side's lanes that no connection holds, below the next never used.
    free: Vec<LaneId>,
    /// The next of this side's lanes never used yet.
    unused: LaneId,
    /// The peer's lanes that carry something now; any other is idle, its next frame an Open or an Accept.
    there: BTreeMap<LaneId, Lane>,
    conns: BTreeMap<Conn, State>,
    /// The next id a connection opened here takes.
    next: u64,
}

impl Mux {
    /// The lanes of one peer connection, serving `serves`.
    pub fn new(limits: Limits, serves: &[u32]) -> Self {
        Self {
            limits,
            serves: serves.to_vec(),
            free: Vec::new(),
            unused: REFUSALS.saturating_add(1),
            there: BTreeMap::new(),
            conns: BTreeMap::new(),
            next: 0,
        }
    }

    /// How many connections are open, in either direction.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// Lowers this side's lanes to `lanes` where they are more (a peer that holds fewer): connections that hold a
    /// lane past it keep it until they are gone.
    pub fn limit(&mut self, lanes: u32) {
        self.limits.lanes = self.limits.lanes.min(lanes);
        self.free.retain(|lane| *lane <= lanes);
    }

    /// This side's lane `conn` sends on, while its half is open: where its next frames queue.
    pub fn our_lane(&self, conn: Conn) -> Option<LaneId> {
        match self.conns.get(&conn)?.here {
            Here::Open(lane) => Some(lane),
            Here::Ended(_) => None,
        }
    }

    /// One of this side's lanes, if any is free.
    fn take_lane(&mut self) -> Option<LaneId> {
        if let Some(lane) = self.free.pop() {
            return Some(lane);
        }
        // Lanes 1..=lanes: lane 0 is the refusals'.
        if self.unused > self.limits.lanes {
            return None;
        }
        let lane = self.unused;
        self.unused = self.unused.checked_add(1)?;
        Some(lane)
    }

    fn give_lane(&mut self, lane: LaneId) {
        if lane != REFUSALS && lane <= self.limits.lanes && !self.free.contains(&lane) {
            self.free.push(lane);
        }
    }

    /// Opens a connection to the peer's `port`: its Open, and bytes may follow at once ([`Mux::send`]); the
    /// peer's answer comes as [`Out::Opened`] or [`Out::Refused`].
    pub fn connect(&mut self, port: u32) -> Result<(Conn, Out), Why> {
        let data = data_kind(port).ok_or(Why::NoPort)?;
        let id = self.next;
        let next = self.next.checked_add(1).ok_or(Why::Conns)?;
        let lane = self.take_lane().ok_or(Why::Lanes)?;
        self.next = next;
        let conn = Conn { here: true, id };
        self.conns.insert(
            conn,
            State::new(port, data, Here::Open(lane), There::Pending),
        );
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&port.to_be_bytes());
        payload.extend_from_slice(&id.to_be_bytes());
        Ok((
            conn,
            Out::Send {
                lane,
                kind: Kind::Open,
                payload,
            },
        ))
    }

    /// `bytes` of `conn` to the peer: the frames that carry them, each at most [`DATA_BOUND`]. Refused once this
    /// side's half has ended, for a connection that is gone, and past the room the peer allows ([`Mux::room`]).
    pub fn send(&mut self, conn: Conn, bytes: &[u8]) -> Result<Vec<Out>, Why> {
        let state = self.conns.get_mut(&conn).ok_or(Why::Gone)?;
        let Here::Open(lane) = state.here else {
            return Err(Why::Gone);
        };
        let len = u64::try_from(bytes.len()).map_err(|_| Why::Credit)?;
        let sent = state.sent.checked_add(len).ok_or(Why::Credit)?;
        if sent > state.allowed {
            return Err(Why::Credit);
        }
        state.sent = sent;
        Ok(bytes
            .chunks(DATA_BOUND)
            .map(|chunk| Out::Send {
                lane,
                kind: state.data,
                payload: chunk.to_vec(),
            })
            .collect())
    }

    /// The bytes of `conn` the peer allows this side to send now.
    pub fn room(&self, conn: Conn) -> u64 {
        self.conns
            .get(&conn)
            .map_or(0, |state| state.allowed.saturating_sub(state.sent))
    }

    /// `conn`'s owner has read `bytes` more: the peer may send that many more. Nothing for a connection gone.
    pub fn grant(&mut self, conn: Conn, bytes: u64) -> Option<Out> {
        let state = self.conns.get_mut(&conn)?;
        if bytes == 0 || their_lane(state.there).is_none() {
            return None;
        }
        state.granted = state.granted.checked_add(bytes)?;
        Some(Out::Send {
            lane: state.here.lane(),
            kind: Kind::Credit,
            payload: bytes.to_be_bytes().to_vec(),
        })
    }

    /// The port `conn` is to.
    pub fn port(&self, conn: Conn) -> Option<u32> {
        self.conns.get(&conn).map(|state| state.port)
    }

    /// Ends this side's half of `conn`: its End, and the connection closed where the peer's half has ended too.
    /// Nothing for a half already ended or a connection gone.
    pub fn end(&mut self, conn: Conn) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(state) = self.conns.get_mut(&conn) else {
            return out;
        };
        let Here::Open(lane) = state.here else {
            return out;
        };
        state.here = Here::Ended(lane);
        out.push(Out::Send {
            lane,
            kind: Kind::End,
            payload: Vec::new(),
        });
        self.close_if_done(conn, &mut out);
        out
    }

    /// Resets `conn` for `why`, its owner's word: both halves end at once, and what the peer sent on it from now
    /// is let go. Nothing for a connection gone.
    pub fn reset(&mut self, conn: Conn, why: Why) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(state) = self.conns.remove(&conn) else {
            return out;
        };
        let lane = state.here.lane();
        out.push(Out::Send {
            lane,
            kind: Kind::Reset,
            payload: vec![why.code()],
        });
        self.give_lane(lane);
        match state.there {
            // The peer's frames already sent are let go until its End or Reset frees the lane.
            There::On(theirs) => {
                self.there.insert(theirs, Lane::Discard);
            }
            // Its End came: nothing more of this connection comes on its lane.
            There::Ended(theirs) => {
                self.there.remove(&theirs);
            }
            There::Pending => {}
        }
        out
    }

    /// The word that this side admitted the peer, on the refusals' lane.
    pub fn welcome(&self) -> Out {
        Out::Send {
            lane: REFUSALS,
            kind: Kind::Welcome,
            payload: Vec::new(),
        }
    }

    /// The connection to the peer closed: every connection on it is reset, and every lane is idle for the next
    /// connection.
    pub fn closed(&mut self) -> Vec<Out> {
        let conns = std::mem::take(&mut self.conns);
        self.there.clear();
        self.free.clear();
        self.unused = REFUSALS.saturating_add(1);
        conns
            .into_keys()
            .map(|conn| Out::Reset {
                conn,
                why: Why::Closed,
            })
            .collect()
    }

    /// Both halves ended: the connection goes, and both its lanes are free again.
    fn close_if_done(&mut self, conn: Conn, out: &mut Vec<Out>) {
        let Some(state) = self.conns.get(&conn) else {
            return;
        };
        let (Here::Ended(mine), There::Ended(theirs)) = (state.here, state.there) else {
            return;
        };
        self.conns.remove(&conn);
        self.give_lane(mine);
        self.there.remove(&theirs);
        out.push(Out::Closed { conn });
    }

    /// A frame of `kind` came on the peer's lane `lane`: what it means. A frame that breaks the protocol is
    /// [`Broken`], and the shell ends the peer's connection.
    pub fn frame(
        &mut self,
        lane: LaneId,
        kind: Kind,
        payload: Vec<u8>,
    ) -> Result<Vec<Out>, Broken> {
        let mut out = Vec::new();
        match kind {
            Kind::Open => self.opened(lane, &payload, &mut out)?,
            Kind::Accept => self.accepted(lane, &payload, &mut out)?,
            Kind::Refuse => self.refused(lane, &payload, &mut out)?,
            Kind::End => self.ended(lane, &mut out)?,
            Kind::Reset => self.was_reset(lane, &payload, &mut out)?,
            Kind::ControlData | Kind::StreamData | Kind::BulkData => {
                self.data(lane, kind, payload, &mut out)?;
            }
            Kind::Credit => self.credited(lane, &payload, &mut out)?,
            Kind::Welcome => {
                if lane != REFUSALS || !payload.is_empty() {
                    return Err(Broken(format!(
                        "a Welcome of {} bytes on lane {lane}",
                        payload.len()
                    )));
                }
                out.push(Out::Welcomed);
            }
        }
        Ok(out)
    }

    /// A lane's next frame names what it carries; one that carries something already breaks the protocol.
    fn idle(&self, lane: LaneId, what: &str) -> Result<(), Broken> {
        if lane == REFUSALS {
            return Err(Broken(format!("{what} on the refusals' lane")));
        }
        if self.there.contains_key(&lane) {
            return Err(Broken(format!(
                "{what} on lane {lane}, which carries a connection"
            )));
        }
        Ok(())
    }

    fn opened(&mut self, lane: LaneId, payload: &[u8], out: &mut Vec<Out>) -> Result<(), Broken> {
        self.idle(lane, "an Open")?;
        let Ok([p0, p1, p2, p3, i0, i1, i2, i3, i4, i5, i6, i7]) = <[u8; 12]>::try_from(payload)
        else {
            return Err(Broken(format!(
                "an Open of {} bytes, not 12",
                payload.len()
            )));
        };
        let port = u32::from_be_bytes([p0, p1, p2, p3]);
        let id = u64::from_be_bytes([i0, i1, i2, i3, i4, i5, i6, i7]);
        let conn = Conn { here: false, id };
        if self.conns.contains_key(&conn) {
            return Err(Broken(format!("an Open of connection {id}, which is open")));
        }
        let refuse = |why: Why| {
            let mut payload = Vec::with_capacity(9);
            payload.extend_from_slice(&id.to_be_bytes());
            payload.push(why.code());
            Out::Send {
                lane: REFUSALS,
                kind: Kind::Refuse,
                payload,
            }
        };
        let data = data_kind(port).filter(|_| self.serves.contains(&port));
        let Some(data) = data else {
            self.there.insert(lane, Lane::Discard);
            out.push(refuse(Why::NoPort));
            return Ok(());
        };
        let Some(mine) = self.take_lane() else {
            self.there.insert(lane, Lane::Discard);
            out.push(refuse(Why::Lanes));
            return Ok(());
        };
        self.there.insert(lane, Lane::Bound(conn));
        self.conns.insert(
            conn,
            State::new(port, data, Here::Open(mine), There::On(lane)),
        );
        out.push(Out::Send {
            lane: mine,
            kind: Kind::Accept,
            payload: id.to_be_bytes().to_vec(),
        });
        out.push(Out::Accepted { conn, port });
        Ok(())
    }

    fn accepted(&mut self, lane: LaneId, payload: &[u8], out: &mut Vec<Out>) -> Result<(), Broken> {
        self.idle(lane, "an Accept")?;
        let id = id_of(payload, "an Accept")?;
        let conn = Conn { here: true, id };
        let Some(state) = self.conns.get_mut(&conn) else {
            // A connection reset here as its Accept came: the lane is let go until the peer's End or Reset.
            if id < self.next {
                self.there.insert(lane, Lane::Discard);
                return Ok(());
            }
            return Err(Broken(format!(
                "an Accept of connection {id}, never opened"
            )));
        };
        if state.there != There::Pending {
            return Err(Broken(format!("a second Accept of connection {id}")));
        }
        state.there = There::On(lane);
        self.there.insert(lane, Lane::Bound(conn));
        out.push(Out::Opened { conn });
        Ok(())
    }

    fn refused(&mut self, lane: LaneId, payload: &[u8], out: &mut Vec<Out>) -> Result<(), Broken> {
        if lane != REFUSALS {
            return Err(Broken(format!(
                "a Refuse on lane {lane}, not the refusals'"
            )));
        }
        let Ok([i0, i1, i2, i3, i4, i5, i6, i7, why]) = <[u8; 9]>::try_from(payload) else {
            return Err(Broken(format!(
                "a Refuse of {} bytes, not 9",
                payload.len()
            )));
        };
        let id = u64::from_be_bytes([i0, i1, i2, i3, i4, i5, i6, i7]);
        let conn = Conn { here: true, id };
        let Some(state) = self.conns.get(&conn) else {
            if id < self.next {
                return Ok(());
            }
            return Err(Broken(format!("a Refuse of connection {id}, never opened")));
        };
        if state.there != There::Pending {
            return Err(Broken(format!("a Refuse of connection {id}, accepted")));
        }
        let here = state.here;
        self.conns.remove(&conn);
        // The peer lets go what this side sent on its lane until that half's End frees it.
        if let Here::Open(mine) = here {
            out.push(Out::Send {
                lane: mine,
                kind: Kind::End,
                payload: Vec::new(),
            });
        }
        self.give_lane(here.lane());
        out.push(Out::Refused {
            conn,
            why: Why::of(why),
        });
        Ok(())
    }

    fn ended(&mut self, lane: LaneId, out: &mut Vec<Out>) -> Result<(), Broken> {
        match self.there.get(&lane).copied() {
            Some(Lane::Discard) => {
                self.there.remove(&lane);
                Ok(())
            }
            Some(Lane::Bound(conn)) => {
                let Some(state) = self.conns.get_mut(&conn) else {
                    self.there.remove(&lane);
                    return Ok(());
                };
                if state.there != There::On(lane) {
                    return Err(Broken(format!("a second End on lane {lane}")));
                }
                state.there = There::Ended(lane);
                out.push(Out::Ended { conn });
                self.close_if_done(conn, out);
                Ok(())
            }
            None => Err(Broken(format!(
                "an End on lane {lane}, which carries nothing"
            ))),
        }
    }

    fn was_reset(
        &mut self,
        lane: LaneId,
        payload: &[u8],
        out: &mut Vec<Out>,
    ) -> Result<(), Broken> {
        let why = match payload {
            [why] => Why::of(*why),
            _ => {
                return Err(Broken(format!("a Reset of {} bytes, not 1", payload.len())));
            }
        };
        match self.there.remove(&lane) {
            // A Reset that crossed this side's close (both halves had ended here) or its own reset: what it ends is
            // gone already. The sender's later frames on the lane follow it, so none of them is taken for another.
            None | Some(Lane::Discard) => Ok(()),
            Some(Lane::Bound(conn)) => {
                if let Some(state) = self.conns.remove(&conn) {
                    // This side's half ends too: the peer lets go its frames until this End.
                    if let Here::Open(mine) = state.here {
                        out.push(Out::Send {
                            lane: mine,
                            kind: Kind::End,
                            payload: Vec::new(),
                        });
                    }
                    self.give_lane(state.here.lane());
                }
                out.push(Out::Reset { conn, why });
                Ok(())
            }
        }
    }

    fn data(
        &mut self,
        lane: LaneId,
        kind: Kind,
        bytes: Vec<u8>,
        out: &mut Vec<Out>,
    ) -> Result<(), Broken> {
        match self.there.get(&lane) {
            Some(Lane::Discard) => Ok(()),
            Some(Lane::Bound(conn)) => {
                let conn = *conn;
                let state = self.conns.get(&conn);
                if state.map(|state| state.there) != Some(There::On(lane)) {
                    return Err(Broken(format!("{kind:?} on lane {lane} after its End")));
                }
                let expected = state.map(|state| state.data);
                if expected != Some(kind) {
                    return Err(Broken(format!(
                        "{kind:?} on a connection whose port's bytes are {expected:?}"
                    )));
                }
                if let Some(state) = self.conns.get_mut(&conn) {
                    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                    state.received = state.received.saturating_add(len);
                    if state.received > state.granted {
                        return Err(Broken(format!(
                            "{} bytes on connection {} past the {} it was granted",
                            state.received, conn.id, state.granted
                        )));
                    }
                }
                if !bytes.is_empty() {
                    out.push(Out::Data { conn, bytes });
                }
                Ok(())
            }
            None => Err(Broken(format!(
                "{kind:?} on lane {lane}, which carries nothing"
            ))),
        }
    }
}

impl Mux {
    fn credited(&mut self, lane: LaneId, payload: &[u8], out: &mut Vec<Out>) -> Result<(), Broken> {
        let bytes = <[u8; 8]>::try_from(payload)
            .map(u64::from_be_bytes)
            .map_err(|_| Broken(format!("a Credit of {} bytes, not 8", payload.len())))?;
        match self.there.get(&lane).copied() {
            // A Credit sent after its sender's End can cross this side's close: the connection it grants is gone.
            None | Some(Lane::Discard) => Ok(()),
            Some(Lane::Bound(conn)) => {
                let Some(state) = self.conns.get_mut(&conn) else {
                    return Ok(());
                };
                state.allowed = state.allowed.checked_add(bytes).ok_or_else(|| {
                    Broken(format!(
                        "a Credit past every byte on connection {}",
                        conn.id
                    ))
                })?;
                out.push(Out::Credit { conn });
                Ok(())
            }
        }
    }
}

/// The peer's lane a half on one names, if it is on one.
fn their_lane(there: There) -> Option<LaneId> {
    match there {
        There::On(lane) => Some(lane),
        There::Pending | There::Ended(_) => None,
    }
}

fn id_of(payload: &[u8], what: &str) -> Result<u64, Broken> {
    <[u8; 8]>::try_from(payload)
        .map(u64::from_be_bytes)
        .map_err(|_| Broken(format!("{what} of {} bytes, not 8", payload.len())))
}

#[cfg(test)]
mod tests;
