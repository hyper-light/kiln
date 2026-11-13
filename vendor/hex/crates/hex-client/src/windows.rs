//! Windows' local client transport (native plan step 8, HEX-RT-17): an AF_UNIX stream, framed messages,
//! and handles the node duplicated into this process, named in each answer's envelope. The one client core
//! owns reads, waits, whole-message sends and the Taken acknowledgement; this transport starts no thread.

use std::os::windows::io::AsSocket;
use std::path::Path;

use hex_afunix::{self as socket, Socket as Raw};
use hex_api::{Class, Refusal};
use hex_core::patience::Patience;
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Received};

use crate::calls::{
    BOX_KINDS, CLASSES, Core, Handle, KINDS, Lent, ReplyDeadline, Sent, Shared, Take, Transport,
    by_class, class_waits,
};

/// The largest wire message plus its envelope's three stream handles and their count.
const FRAME_BOUND: usize = wire::MAX_MESSAGE + socket::ENVELOPE_MAX;

/// Each call kind learns its wait on this Windows node, as on the other native transports (decision 9); the kinds a
/// box's class times are [`CLASS_ANSWERS`].
static ANSWERS: [Patience; KINDS] = [
    Patience::measuring("client.answer.hello"),
    Patience::measuring("client.answer.list"),
    Patience::measuring("client.answer.netns"),
    Patience::measuring("client.answer.schedule.describe"),
    Patience::measuring("client.answer.schedule.advertise"),
    Patience::measuring("client.answer.schedule.submit"),
    Patience::measuring("client.answer.schedule.hear_create"),
    Patience::measuring("client.answer.schedule.hear_delete"),
    Patience::measuring("client.answer.schedule.hear_control"),
    Patience::measuring("client.answer.schedule.link_up"),
    Patience::measuring("client.answer.schedule.retry"),
    Patience::measuring("client.answer.schedule.delete"),
    Patience::measuring("client.answer.schedule.status"),
    Patience::measuring("client.answer.physical_report"),
    Patience::measuring("client.answer.initialize_backing"),
    Patience::measuring("client.answer.release_backing"),
    Patience::measuring("client.answer.backing_usage"),
    Patience::measuring("client.answer.schedule.forget"),
];

/// AC-T7.4.1: the kinds a box's class times, a wait for each (kind, class), named `client.answer.<kind>.<class>`; no
/// census has timed any here: MEASURING, each, until one records it.
static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.answer", "process", measuring),
    class_waits!("client.answer", "oci", measuring),
    class_waits!("client.answer", "firecracker", measuring),
    class_waits!("client.answer", "shards", measuring),
];

pub type Client = Core<Socket>;

/// A non-blocking AF_UNIX connection to the local node agent.
pub struct Socket {
    raw: Raw,
}

pub struct Reading {
    buf: Vec<u8>,
    deframer: Deframer,
    reset: Option<Refusal>,
}

impl Transport for Socket {
    type Reading = Reading;

    fn answers() -> &'static [Patience; KINDS] {
        &ANSWERS
    }

    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        by_class(&CLASS_ANSWERS, class)
    }

    fn reading(&self) -> Reading {
        Reading {
            buf: vec![0; stream::MAX_FRAME],
            deframer: Deframer::new(FRAME_BOUND),
            reset: None,
        }
    }

    fn handle(&self) -> Handle<'_> {
        self.raw.as_socket()
    }

    fn open_offer(&self, offer: &wire::StreamOffer) -> Result<hex_api::Given, Refusal> {
        crate::guest::open_offered(offer, None)
    }

    fn open_offer_before(
        &self,
        offer: &wire::StreamOffer,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        crate::guest::open_offered(offer, Some(deadline))
    }

    fn recv(&self, reading: &mut Reading, take: &mut Take<'_>) -> Result<bool, Refusal> {
        let n = match self.raw.recv(&mut reading.buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Ok(false);
            }
            Err(e) if socket::is_reset(&e) && reading.reset.is_none() => {
                reading.reset = Some(Refusal::Broken(
                    "the node agent closed the connection with requests unread".to_owned(),
                ));
                return Ok(true);
            }
            Err(e) => return Err(Refusal::os("reading the node agent's socket", e)),
        };
        if n == 0 {
            return Err(reading.reset.take().unwrap_or_else(|| {
                Refusal::Broken("the node agent closed the connection".to_owned())
            }));
        }
        let mut input = reading.buf.get(..n).unwrap_or_default();
        while !input.is_empty() {
            let (took, whole) = reading.deframer.take(input).map_err(|fault| {
                Refusal::Broken(format!("the node agent's stream can't be read: {fault}"))
            })?;
            input = input.get(took..).unwrap_or_default();
            let Some(payload) = whole else { continue };
            // The server keeps its copies until the common core sends Taken. The duplicated ends named by
            // this envelope become ours here, and drop with an answer that doesn't take them.
            let (ends, message) = socket::take_given(&payload)
                .map_err(|e| Refusal::os("taking the node agent's given stream handles", e))?;
            let received = Received {
                bytes: message,
                size: message.len(),
                descriptors: ends.len(),
                descriptors_cut: false,
            };
            take(
                &received,
                ends.into_iter().map(hex_api::End::from).collect(),
            )?;
        }
        Ok(true)
    }

    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        stream::frame(&message, wire::MAX_MESSAGE).map_err(|fault| Refusal::Invalid {
            what: "message",
            why: fault.to_string(),
        })
    }

    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        if !lent.is_empty() {
            return Err(Sent::Refused(Refusal::Platform {
                what: "an exec's stdio through the node agent's socket",
                platform: std::env::consts::OS,
                why: "Windows' AF_UNIX passes no handles; ask the node for the exec's streams instead of lending a pipe".to_owned(),
            }));
        }
        while *at < bytes.len() {
            match self.raw.send(bytes.get(*at..).unwrap_or_default()) {
                Ok(0) => return Err(Sent::Gone),
                Ok(n) => *at = at.saturating_add(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(e)
                    if socket::is_reset(&e)
                        || matches!(
                            e.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected
                        ) =>
                {
                    return Err(Sent::Gone);
                }
                Err(e) => return Err(Sent::Refused(Refusal::os("sending to the node agent", e))),
            }
        }
        Ok(true)
    }

    fn shutdown(&self) {
        self.raw.shutdown();
    }
}

impl Client {
    /// Connects to the node agent, then gives the non-blocking socket to the common reader's turn.
    pub fn connect(path: &Path) -> Result<Self, Refusal> {
        let raw = Raw::connect(path).map_err(|e| Refusal::Os {
            op: "connecting to the node agent's socket",
            why: format!("{}: {e}", path.display()),
        })?;
        raw.set_nonblocking(true)
            .map_err(|e| Refusal::os("making the node agent's socket non-blocking", e))?;
        Core::hello(Shared::try_new(Socket { raw })?)
    }
}

#[cfg(test)]
mod tests;
