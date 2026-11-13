//! CP-T7.4/LB03: the live scheduler, with the same driver in laptop and peer modes.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use hex_api::Refusal;
use hex_core::schedule::{Command, Forget};
use hex_core::wire;

const USAGE: &str = "usage: hex schedule serve --config <file> | hex schedule call --socket <path> --request <file> | hex schedule tunnel --socket <path> --from <node> | hex schedule forget --socket <path> --holder <name>";

pub fn main(args: &[String]) -> u8 {
    match run(args) {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "hex schedule: {why}");
            1
        }
    }
}

fn run(args: &[String]) -> Result<(), Refusal> {
    hex_node::census::install_from_env()?;
    let invalid = || Refusal::Invalid {
        what: "schedule arguments",
        why: USAGE.into(),
    };
    match args {
        [verb, flag, file] if verb == "serve" && flag == "--config" => {
            let config = hex_node::scheduler::read_config(Path::new(file))?;
            let own = config.node;
            let service = hex_node::scheduler::Service::open(
                &config,
                Arc::new(move |node, why| {
                    let mut err = std::io::stderr().lock();
                    let _ = if node.0 == own {
                        writeln!(err, "hex scheduler: {why}")
                    } else {
                        writeln!(err, "hex scheduler peer {}: {why}", node.0)
                    };
                }),
            )?;
            // The capacity view's lane, where the configuration asks for one (joint plan step 13): its address first,
            // so a port of 0 is told.
            if let Some(address) = service.capacity_address() {
                writeln!(std::io::stdout(), "capacity {address}")
                    .map_err(|e| Refusal::os("reporting the capacity lane", e))?;
            }
            #[cfg(target_os = "linux")]
            let platform = hex_node::sizes::Sizes::now()?.platform();
            #[cfg(target_os = "macos")]
            let platform = hex_node::serve::macos::platform()?;
            #[cfg(windows)]
            let platform = hex_node::serve::windows::platform()?;
            let host = Arc::clone(&service) as Arc<dyn hex_node::serve::Host>;
            let mut server = hex_node::serve::Server::bind(
                &config.socket,
                host,
                hex_node::serve::limits(&platform),
                platform.threads,
            )?;
            writeln!(std::io::stdout(), "ready {}", config.socket.display())
                .map_err(|e| Refusal::os("reporting scheduler readiness", e))?;
            let fault = server.wait();
            service.shutdown()?;
            match fault {
                Some(why) => Err(Refusal::Broken(why)),
                None => Ok(()),
            }
        }
        [verb, socket_flag, socket, request_flag, request]
            if verb == "call" && socket_flag == "--socket" && request_flag == "--request" =>
        {
            let command: Command = read_command(Path::new(request))?;
            let reply = hex_client::Client::connect(Path::new(socket))?.schedule(command)?;
            let text = serde_json::to_vec(&reply).map_err(|e| Refusal::Broken(e.to_string()))?;
            let mut output = std::io::stdout().lock();
            output
                .write_all(&text)
                .and_then(|()| output.write_all(b"\n"))
                .map_err(|e| Refusal::os("writing scheduler reply", e))
        }
        [verb, socket_flag, socket, holder_flag, holder]
            if verb == "forget" && socket_flag == "--socket" && holder_flag == "--holder" =>
        {
            forget(Path::new(socket), holder)
        }
        [verb, flag, socket, from_flag, from]
            if verb == "tunnel" && flag == "--socket" && from_flag == "--from" =>
        {
            if std::env::var_os("SSH_ORIGINAL_COMMAND").is_none() {
                return Err(Refusal::Invalid {
                    what: "peer identity",
                    why: "use an SSH key whose forced tunnel command fixes --from for that peer"
                        .into(),
                });
            }
            let from = from.parse::<u32>().map_err(|e| Refusal::Invalid {
                what: "peer identity",
                why: e.to_string(),
            })?;
            hex_node::scheduler::tunnel::serve(
                Path::new(socket),
                from,
                &mut std::io::stdin().lock(),
                &mut std::io::stdout().lock(),
            )
        }
        _ => Err(invalid()),
    }
}

/// `hex schedule forget --socket <path> --holder <name>`: the person's release of a capacity holder gone for good
/// (joint plan step 13, `docs/design/capacity-view.md` CV6), which the scheduler logs as `hex.capacity
/// event=forgotten`. Says `released <name> ...` with what it released; refused, typed, while the holder is connected
/// (its own sync says what it holds), for a name never seen, and where the scheduler keeps no capacity view.
fn forget(socket: &Path, holder: &str) -> Result<(), Refusal> {
    let command = Command::Forget {
        holder: holder.to_owned(),
    };
    let reply = hex_client::Client::connect(socket)?.schedule(command)?;
    let release = |why: String| Refusal::Invalid {
        what: "capacity holder's release",
        why,
    };
    match reply.forget {
        Some(Forget::Released {
            holder,
            epoch,
            count,
            sum,
        }) => writeln!(
            std::io::stdout(),
            "released {holder} epoch={epoch} count={count} cpu_milli={} mem_bytes={} disk_bytes={} gpus={}",
            sum.cpu_milli,
            sum.mem_bytes,
            sum.disk_bytes,
            sum.gpus
        )
        .map_err(|e| Refusal::os("reporting the release", e)),
        Some(Forget::Connected) => Err(release(format!(
            "{holder} is connected: its own sync says what it holds"
        ))),
        Some(Forget::Unknown) => Err(release(format!(
            "no holder named {holder} was ever here"
        ))),
        None => Err(Refusal::Broken(
            "the scheduler's answer to a release says nothing of it".into(),
        )),
    }
}

fn read_command(path: &Path) -> Result<Command, Refusal> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| Refusal::os("opening scheduler request", e))?
        .take(
            u64::try_from(wire::MAX_MESSAGE)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|e| Refusal::os("reading scheduler request", e))?;
    if bytes.len() > wire::MAX_MESSAGE {
        return Err(Refusal::Exhausted {
            what: "scheduler request bytes",
            bound: wire::MAX_MESSAGE,
        });
    }
    serde_json::from_slice(&bytes).map_err(|e| Refusal::Invalid {
        what: "scheduler request",
        why: e.to_string(),
    })
}
