//! kiln's agents as kiln-seams' `HarnessPort` (docs/seams.md, "HarnessPort"): the hub's harness
//! registry, its own sessions and every runner's. The goal driver reaches agents through it (and
//! its tests through `kiln_seams::harness::SimHarness`).
//!
//! Delivered means the agent's inbox took the message (CP-T3.2: once, whatever retried it; a repeat
//! is answered as the first was), or, from a runner from before inboxes, that it's on its way to the
//! agent's machine. Acknowledged end to end, parked for a machine that's away, is CP-T3.3's.

use futures::future::BoxFuture;
use kiln_seams::harness::Sent;
use serde_json::Value;

use super::inbox::Refusal;
use super::model::Command;
use super::registry;
use super::session::Info;
use crate::hub::Hub;

impl kiln_seams::HarnessPort for Hub {
    type Agent = Info;
    type Command = Command;

    fn agents(&self) -> Vec<Info> {
        self.harness.list()
    }

    fn agent(&self, id: &str) -> Option<Info> {
        self.harness.info(id)
    }

    fn catalog(&self, id: &str) -> Option<Value> {
        self.harness.meta(id)
    }

    fn send<'a>(&'a self, id: &'a str, command: Command) -> BoxFuture<'a, Sent> {
        Box::pin(async move {
            from_registry(self.harness.command(self, id, command).await)
        })
    }

    fn send_status<'a>(&'a self, id: &'a str, command: Command, key: &'a str) -> BoxFuture<'a, Sent> {
        Box::pin(async move {
            from_registry(self.harness.command_status(self, id, command, super::mail::StatusKey { task: None, key: key.to_owned() }).await)
        })
    }
}

fn from_registry(sent: registry::Sent) -> Sent {
    match sent {
                registry::Sent::Published(receipt) => match receipt.disposition {
                    super::mail::Disposition::Admitted | super::mail::Disposition::Consumed => Sent::Delivered,
                    super::mail::Disposition::Waiting | super::mail::Disposition::Dispatched | super::mail::Disposition::Transferred => Sent::Retained,
                    super::mail::Disposition::Cancelled | super::mail::Disposition::Superseded => Sent::Cancelled,
                },
                registry::Sent::PublicationRefused(super::mail::Error::Capacity { .. } | super::mail::Error::Backpressure { .. } | super::mail::Error::StorageFull) => Sent::Full,
                registry::Sent::PublicationRefused(super::mail::Error::NoSession) => Sent::NoSession,
                registry::Sent::PublicationRefused(super::mail::Error::Invalid { .. } | super::mail::Error::Reused { .. } | super::mail::Error::Unavailable { .. } | super::mail::Error::Stale { .. } | super::mail::Error::TooLarge { .. }) => Sent::Unreachable,
                registry::Sent::Ok | registry::Sent::Accepted(_) | registry::Sent::Taken | registry::Sent::Parked => Sent::Delivered,
                registry::Sent::Rejected { code: crate::wire::channel::Refusal::Unknown, .. } => Sent::NoSession,
                registry::Sent::Rejected { .. } | registry::Sent::Unanswered(_) => Sent::Unreachable,
                // Its id was given to another message before: one sent under it got there (a retry
                // whose words changed since its first try).
                registry::Sent::Refused(Refusal::Reused { .. }) => Sent::Delivered,
                registry::Sent::Refused(Refusal::Full { .. }) => Sent::Full,
                registry::Sent::Refused(Refusal::NoSession { .. }) | registry::Sent::NoSession => Sent::NoSession,
                registry::Sent::Refused(_) | registry::Sent::Unreachable => Sent::Unreachable,
            }
}

/// kiln's registry against kiln-seams' HarnessPort suite, every clause, on sessions of the hub's own
/// machine whose harness is a program that isn't there: what reached an agent is what its inbox
/// took (CP-T3.2).
#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::BoxFuture;
    use kiln_seams::HarnessPort;
    use kiln_seams::harness::{HarnessFixture, Sent, cases};
    use parking_lot::Mutex;
    use serde_json::Value;

    use crate::harness::model::{Command, SendMode};
    use crate::harness::session::{Info, Spec};
    use crate::hub::Hub;

    /// The registry, its agents under the names the cases give them (it names its own).
    struct Named {
        hub: Arc<Hub>,
        ids: Mutex<HashMap<String, String>>,
        /// The words of each message said, by its id.
        said: Mutex<HashMap<String, String>>,
        heard: Mutex<HashMap<String, std::path::PathBuf>>,
    }

    impl Named {
        fn real(&self, name: &str) -> String {
            self.ids.lock().get(name).cloned().unwrap_or_else(|| name.to_owned())
        }
    }

    impl HarnessPort for Named {
        type Agent = Info;
        type Command = Command;
        fn agents(&self) -> Vec<Info> {
            self.hub.agents()
        }
        fn agent(&self, name: &str) -> Option<Info> {
            HarnessPort::agent(self.hub.as_ref(), &self.real(name))
        }
        fn catalog(&self, name: &str) -> Option<Value> {
            self.hub.catalog(&self.real(name))
        }
        fn send<'a>(&'a self, name: &'a str, command: Command) -> BoxFuture<'a, Sent> {
            let id = self.real(name);
            Box::pin(async move {
                let message = crate::harness::host::message_of(&command).map(|(message, _)| message);
                let sent = HarnessPort::send(self.hub.as_ref(), &id, command).await;
                if sent.has_custody() {
                    // Preserve the returned custody state. Only an exact native result can
                    // release credit before the next conformance send; HTTP retention cannot.
                    // The fixture's own steps (an original ID, its correlated successful result
                    // committed) failing fail the case they serve: it hears no custody.
                    let Some(message) = message else { return Sent::Unreachable };
                    if crate::harness::ops::end_to_end::source_disposition(&self.hub, &id, &message, true).await.is_err() {
                        return Sent::Unreachable;
                    }
                }
                sent
            })
        }
    }

    struct RegistryFixture {
        port: Named,
        /// Where its agents work.
        cwd: std::path::PathBuf,
    }

    impl HarnessFixture for RegistryFixture {
        type Port = Named;
        fn port(&self) -> &Named {
            &self.port
        }
        fn open<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let cwd = self.cwd.join(name);
                let (program, heard) = crate::harness::ops::end_to_end::recording_program(&cwd, true).map_err(|error| format!("{error:#}"))?;
                let spec = Spec {
                    kind: "claude".into(),
                    name: Some(name.to_owned()),
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                    program: Some(program.to_string_lossy().into_owned()),
                    ..Spec::default()
                };
                let hub = &self.port.hub;
                let opened = hub.harness.open(hub, "local", spec, 1).await.map_err(|e| format!("{e:#}"))?;
                let info = opened.into_iter().next().ok_or("nothing opened")?;
                self.port.ids.lock().insert(name.to_owned(), info.id.clone());
                self.port.heard.lock().insert(info.id.clone(), heard);
                crate::harness::ops::end_to_end::source_ready(hub, &info.id).await.map_err(|error| format!("{error:#}"))?;
                Ok(())
            })
        }
        fn close<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let hub = &self.port.hub;
                if hub.harness.close(hub, &self.port.real(name)).await { Ok(()) } else { Err(format!("{name} wasn't closed")) }
            })
        }
        fn say(&self, text: &str) -> Command {
            let id = crate::util::new_id();
            self.port.said.lock().insert(id.clone(), text.to_owned());
            Command::Send { id, text: text.to_owned(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }
        }
        fn heard<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<Vec<String>>> {
            Box::pin(async move {
                let id = self.port.real(name);
                let path = self.port.heard.lock().get(&id).cloned()?;
                let messages = crate::harness::ops::end_to_end::prompts(&path);
                let said = self.port.said.lock();
                Some(messages.iter().map(|message| said.get(message).cloned().unwrap_or_else(|| message.clone())).collect())
            })
        }
        fn id_of(&self, agent: &Info) -> String {
            self.port.ids.lock().iter().find(|(_, id)| **id == agent.id).map_or_else(|| agent.id.clone(), |(name, _)| name.clone())
        }
    }

    /// Native successful results settle each exact publication. HARNESS-3 and -5 still
    /// prove actual native order, exactly once and recipient isolation from the subprocess log;
    /// the port's returned Retained/Delivered state is never rewritten by the fixture.
    #[test]
    fn kiln_s_registry_holds_every_harness_port_clause() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let cases = cases::<RegistryFixture>();
            // A hub of its own for each case.
            let hubs = futures::future::join_all(cases.iter().map(|_| crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true))).await;
            let mut hubs: std::collections::VecDeque<Arc<Hub>> = hubs.into_iter().collect::<anyhow::Result<_>>()?;
            // Each takes what its own sessions say (their inboxes' answers among it), as a server does.
            for hub in &hubs {
                let h = Arc::clone(hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::registry::Registry::run_local(h).await;
                });
                let h = Arc::clone(hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::mail_delivery::run(h).await;
                });
            }
            // Their folders, removed once it's done (a scratch hub leaves its own behind).
            let folders: Vec<std::path::PathBuf> = hubs.iter().map(|h| h.cfg.server.data_dir.clone()).collect();
            let last = hubs.back().cloned().ok_or_else(|| anyhow::anyhow!("no cases"))?;
            let dir = tempfile::tempdir()?;
            let make = || {
                // There are as many hubs as cases: the last is never handed out twice.
                let hub = hubs.pop_front().unwrap_or_else(|| Arc::clone(&last));
                let cwd = dir.path().join(crate::util::new_id());
                async move { RegistryFixture { port: Named { hub, ids: Mutex::new(HashMap::new()), said: Mutex::new(HashMap::new()), heard: Mutex::new(HashMap::new()) }, cwd } }
            };
            // Each case fails, named, if the registry hasn't answered it in 30 s (by kiln's runtime).
            let clock = crate::rt::clock();
            let report = kiln_seams::conformance::run_within("HarnessPort (kiln's registry)", &cases, make, || clock.sleep(Duration::from_secs(30))).await;
            drop(last);
            let tmp = std::env::temp_dir();
            for f in folders.iter().filter(|f| f.starts_with(&tmp) && *f != &tmp) {
                let _ = std::fs::remove_dir_all(f);
            }
            anyhow::ensure!(report.failed.is_empty() && report.passed.len() == cases.len(), "held: {:?}, failed: {:?}", report.passed, report.failed);
            Ok(())
        })
    }
}
