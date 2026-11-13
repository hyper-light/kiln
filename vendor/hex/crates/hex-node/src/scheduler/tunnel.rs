//! Authenticated peer shell (LB03): SSH supplies transport identity/encryption; Hex supplies its
//! existing hello, wire identity, bounded frames and typed calls. No submitted text becomes a command.

use std::io::{Read, Write};
use std::path::Path;
// On macOS std's spawn passes no POSIX_SPAWN_CLOEXEC_DEFAULT: ssh starts through Hex's, holding only its stdio.
#[cfg(target_os = "macos")]
use hex_runtime::macos::command::{ChildStdin, ChildStdout, Command, Stdio};
#[cfg(not(target_os = "macos"))]
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use hex_api::Refusal;
use hex_core::{schedule, stream, wire};

use super::Peer;
use super::stopping::{OwnedChild, Stopping};

fn io(op: &'static str) -> impl Fn(std::io::Error) -> Refusal {
    move |e| Refusal::os(op, e)
}

pub fn read_frame(input: &mut impl Read) -> Result<Option<Vec<u8>>, Refusal> {
    let mut prefix = [0u8; size_of::<u32>()];
    // Clean EOF is allowed only between frames. A partial prefix/body is a broken connection.
    let (first, rest) = prefix
        .split_first_mut()
        .ok_or_else(|| Refusal::Broken("empty frame prefix".into()))?;
    loop {
        match input.read(std::slice::from_mut(first)) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io("reading peer frame")(e)),
        }
    }
    input
        .read_exact(rest)
        .map_err(io("reading peer frame prefix"))?;
    let count =
        usize::try_from(u32::from_be_bytes(prefix)).map_err(|e| Refusal::Broken(e.to_string()))?;
    if count > wire::MAX_MESSAGE {
        return Err(Refusal::Exhausted {
            what: "peer message bytes",
            bound: wire::MAX_MESSAGE,
        });
    }
    let mut bytes = vec![0; count];
    input
        .read_exact(&mut bytes)
        .map_err(io("reading peer message"))?;
    Ok(Some(bytes))
}

pub fn write_frame(output: &mut impl Write, bytes: &[u8]) -> Result<(), Refusal> {
    output
        .write_all(&stream::frame(bytes, wire::MAX_MESSAGE).map_err(Refusal::from)?)
        .and_then(|()| output.flush())
        .map_err(io("writing peer message"))
}

fn answer(bytes: &[u8]) -> Result<(wire::Header, wire::Answer), Refusal> {
    wire::read_answer(
        &wire::Received {
            bytes,
            size: bytes.len(),
            descriptors: 0,
            descriptors_cut: false,
        },
        wire::MAX_MESSAGE,
    )
    .map_err(Into::into)
}

/// One bounded ordered command lane per configured peer. Runtime program messages and stdio keep
/// using hex-client's common reader; this tunnel forwards only scheduler control to that reader.
struct ChildGuard {
    child: OwnedChild,
    stopping: Arc<Stopping>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stopping.finish_child(&self.child);
    }
}

pub struct Connection {
    owner: ChildGuard,
    input: ChildStdin,
    output: ChildStdout,
    next: u64,
}

impl Connection {
    pub fn open(peer: &Peer, stopping: &Arc<Stopping>) -> Result<Self, Refusal> {
        if peer.host.is_empty()
            || peer.host.starts_with('-')
            || peer.host.contains(['\n', '\r', '\0'])
            || peer.command.is_empty()
        {
            return Err(Refusal::Invalid {
                what: "SSH peer",
                why: "needs a configured host and remote tunnel command".into(),
            });
        }
        let child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "--",
                &peer.host,
                &peer.command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(io("opening authenticated peer tunnel"))?;
        let owner = ChildGuard {
            child: Arc::new(Mutex::new(child)),
            stopping: Arc::clone(stopping),
        };
        let (input, output) = {
            let mut child = owner
                .child
                .lock()
                .map_err(|_| Refusal::Broken("SSH child lock is poisoned".into()))?;
            let input = child
                .stdin
                .take()
                .ok_or_else(|| Refusal::Broken("SSH stdin was not piped".into()))?;
            let output = child
                .stdout
                .take()
                .ok_or_else(|| Refusal::Broken("SSH stdout was not piped".into()))?;
            (input, output)
        };
        // The service can cancel this child before either half of hello blocks on its pipes.
        stopping
            .register(peer.node, &owner.child)
            .map_err(io("registering SSH peer child"))?;
        let mut conn = Self {
            owner,
            input,
            output,
            next: 0,
        };
        let bytes = wire::request(
            0,
            &wire::Request::Hello {
                build: wire::build(),
            },
            wire::MAX_MESSAGE,
        )
        .map_err(Refusal::from)?;
        write_frame(&mut conn.input, &bytes)?;
        let bytes = read_frame(&mut conn.output)?
            .ok_or_else(|| Refusal::Broken("SSH peer ended before hello".into()))?;
        match answer(&bytes)? {
            (wire::Header { id: 0, .. }, wire::Answer::Hello(_)) => Ok(conn),
            (_, wire::Answer::Refused(why)) => Err(why),
            _ => Err(Refusal::Broken("peer did not answer its hello".into())),
        }
    }

    /// Outer failures mean the connection is broken/uncertain; an inner refusal is a complete,
    /// typed application response, so the worker can proceed to newer queued control.
    pub fn call(
        &mut self,
        command: schedule::Command,
    ) -> Result<Result<schedule::Reply, Refusal>, Refusal> {
        if self.owner.stopping.is_stopped() {
            return Err(Refusal::os(
                "calling stopped scheduler tunnel",
                std::io::Error::new(std::io::ErrorKind::Interrupted, "scheduler stopped"),
            ));
        }
        self.next = self.next.checked_add(1).ok_or(Refusal::Exhausted {
            what: "peer request IDs",
            bound: usize::MAX,
        })?;
        let bytes = wire::request(
            self.next,
            &wire::Request::Call(wire::Call::Schedule(Box::new(command))),
            wire::MAX_MESSAGE,
        )
        .map_err(Refusal::from)?;
        write_frame(&mut self.input, &bytes)?;
        let bytes = read_frame(&mut self.output)?
            .ok_or_else(|| Refusal::Broken("peer tunnel closed".into()))?;
        let (header, answer) = answer(&bytes)?;
        if header.id != self.next {
            return Err(Refusal::Broken("peer answered a different request".into()));
        }
        match answer {
            wire::Answer::Scheduled(reply) => Ok(Ok(*reply)),
            wire::Answer::Refused(why) => Ok(Err(why)),
            _ => Err(Refusal::Broken(
                "peer did not answer scheduler control".into(),
            )),
        }
    }
}

/// SSH's forced command binds this sender to its credential. Local administration never crosses
/// this endpoint, and a peer cannot claim another configured node's identity inside a command.
fn peer_command_allowed(command: &schedule::Command, allowed_sender: u32) -> bool {
    matches!(command,
        schedule::Command::Hear { from, .. }
        | schedule::Command::Describe { from, .. }
        | schedule::Command::Advertise { from, .. }
        if *from == allowed_sender)
}

/// Remote end of SSH's authenticated stdio channel. The local socket enforces its normal owner
/// credentials and every call uses hex-client's common reader and worker admission.
pub fn serve(
    path: &Path,
    allowed_sender: u32,
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), Refusal> {
    let client = hex_client::Client::connect(path)?;
    let mut greeted = false;
    while let Some(bytes) = read_frame(input)? {
        let (id, answer, finish) = match wire::read_request_apart(&bytes, wire::MAX_MESSAGE) {
            Ok((header, wire::Request::Hello { .. })) if !greeted => {
                greeted = true;
                (
                    header.id,
                    wire::Answer::Hello(wire::Hello {
                        build: wire::build(),
                        classes: Vec::new(),
                        limits: client.limits(),
                        budgets: client.budgets(),
                    }),
                    false,
                )
            }
            Err(wire::Bad {
                id,
                fault: wire::Fault::Wire { .. },
            }) => (
                id,
                wire::Answer::Hello(wire::Hello {
                    build: wire::build(),
                    classes: Vec::new(),
                    limits: client.limits(),
                    budgets: client.budgets(),
                }),
                true,
            ),
            Ok((header, wire::Request::Call(wire::Call::Schedule(command))))
                if greeted && peer_command_allowed(&command, allowed_sender) =>
            {
                (
                    header.id,
                    client
                        .schedule(*command)
                        .map(|reply| wire::Answer::Scheduled(Box::new(reply)))
                        .unwrap_or_else(wire::Answer::Refused),
                    false,
                )
            }
            Ok((header, _)) => (
                header.id,
                wire::Answer::Refused(Refusal::Invalid {
                    what: "scheduler tunnel",
                    why: format!(
                        "hello then Hear, Describe or Advertise from credential-bound peer {allowed_sender} are required"
                    ),
                }),
                true,
            ),
            Err(bad) => (bad.id, wire::Answer::Refused(bad.fault.into()), true),
        };
        write_frame(
            output,
            &wire::answer(id, &answer, wire::MAX_MESSAGE).map_err(Refusal::from)?,
        )?;
        if finish {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lb03_peer_frame_refuses_length_before_body() {
        let count = u32::try_from(wire::MAX_MESSAGE + 1).unwrap().to_be_bytes();
        assert!(matches!(
            read_frame(&mut &count[..]),
            Err(Refusal::Exhausted { .. })
        ));
        assert!(read_frame(&mut &count[..2]).is_err());
        assert_eq!(read_frame(&mut &[][..]).unwrap(), None);
    }
    #[test]
    fn lb03_peer_frame_is_the_existing_control_frame() {
        let bytes = wire::request(
            7,
            &wire::Request::Hello {
                build: wire::build(),
            },
            wire::MAX_MESSAGE,
        )
        .unwrap();
        let mut framed = Vec::new();
        write_frame(&mut framed, &bytes).unwrap();
        assert_eq!(framed, stream::frame(&bytes, wire::MAX_MESSAGE).unwrap());
        assert_eq!(read_frame(&mut &framed[..]).unwrap(), Some(bytes));
    }

    /// LB03: the credential permits peer control from exactly one sender. Local admin actions
    /// cannot use this shell, even after hello, and a foreign sender cannot borrow its credential.
    #[test]
    fn lb03_tunnel_binds_peer_commands_to_the_authenticated_sender() {
        for from in [7, 8] {
            let commands = [
                schedule::Command::Describe {
                    from,
                    cluster: "test".into(),
                },
                schedule::Command::Advertise {
                    cluster: "test".into(),
                    from,
                    offer: schedule::Offer {
                        spec: String::new(),
                        models: Vec::new(),
                    },
                    observations: Vec::new(),
                },
                schedule::Command::Hear {
                    from,
                    packet: schedule::Packet {
                        cluster: "test".into(),
                        epoch: 1,
                        seq: 1,
                        observations: Vec::new(),
                        message: String::new(),
                        payload: None,
                        refusal: None,
                    },
                },
            ];
            for command in commands {
                assert_eq!(peer_command_allowed(&command, 7), from == 7);
            }
        }
        let key = schedule::Key {
            owner: 7,
            epoch: 1,
            seq: 1,
        };
        let payload = schedule::Payload {
            spec: hex_api::BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: hex_api::BoxId::new("tunnel-admin").unwrap(),
                bounds: hex_api::Bounds::default(),
                class: hex_api::ClassSpec::Process,
                owner: None,
                band: 0,
            },
            start: true,
        };
        for command in [
            schedule::Command::Submit {
                demand: String::new(),
                payload,
            },
            schedule::Command::Delete { key },
            schedule::Command::Status { key },
            schedule::Command::Retry,
            schedule::Command::LinkUp { peer: 7 },
        ] {
            assert!(!peer_command_allowed(&command, 7));
        }
    }
}
