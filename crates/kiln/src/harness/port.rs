//! kiln's agents as kiln-seams' `HarnessPort` (docs/seams.md, "HarnessPort"): the hub's harness
//! registry, its own sessions and every runner's. The goal driver reaches agents through it (and
//! its tests through `kiln_seams::harness::SimHarness`).
//!
//! Delivered here means what the registry means by it today: the command is on its way to the
//! session's runner. That every command reaches its agent once, in order, acknowledged end to end,
//! is CP-T3.2's and CP-T3.3's (sends deduplicated, then acknowledged); until they land, the registry
//! passes the port's clauses but HARNESS-3 and HARNESS-5 (`tests` below).

use futures::future::BoxFuture;
use kiln_seams::harness::Sent;
use serde_json::Value;

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
            match self.harness.command(self, id, command).await {
                registry::Sent::Ok => Sent::Delivered,
                registry::Sent::NoSession => Sent::NoSession,
                registry::Sent::Unreachable => Sent::Unreachable,
            }
        })
    }
}

/// kiln's registry against kiln-seams' HarnessPort suite: the clauses it holds today (HARNESS-1,
/// HARNESS-2, HARNESS-4), on sessions of the hub's own machine whose harness is a program that
/// isn't there. HARNESS-3 and HARNESS-5 wait on CP-T3.2 and CP-T3.3.
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

    /// The clauses kiln's registry holds until sends are acknowledged end to end.
    const HOLDS_TODAY: [&str; 3] = ["HARNESS-1", "HARNESS-2", "HARNESS-4"];
    /// The clauses that need what reached an agent, which isn't seen here until then (CP-T3.2,
    /// CP-T3.3).
    const WAIT_FOR_T3: [&str; 2] = ["HARNESS-3", "HARNESS-5"];

    /// The registry, its agents under the names the cases give them (it names its own).
    struct Named {
        hub: Arc<Hub>,
        ids: Mutex<HashMap<String, String>>,
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
            Box::pin(async move { HarnessPort::send(self.hub.as_ref(), &id, command).await })
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
                let spec = Spec {
                    kind: "claude".into(),
                    name: Some(name.to_owned()),
                    cwd: Some(self.cwd.to_string_lossy().into_owned()),
                    program: Some(self.cwd.join("no-such-claude").to_string_lossy().into_owned()),
                    ..Spec::default()
                };
                let hub = &self.port.hub;
                let opened = hub.harness.open(hub, "local", spec, 1).await.map_err(|e| format!("{e:#}"))?;
                let info = opened.into_iter().next().ok_or("nothing opened")?;
                self.port.ids.lock().insert(name.to_owned(), info.id);
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
            Command::Send { id: crate::util::new_id(), text: text.to_owned(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None }
        }
        fn heard<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Option<Vec<String>>> {
            // What reached the agent isn't seen here until sends are acknowledged (CP-T3.3).
            Box::pin(async { None })
        }
        fn id_of(&self, agent: &Info) -> String {
            self.port.ids.lock().iter().find(|(_, id)| **id == agent.id).map_or_else(|| agent.id.clone(), |(name, _)| name.clone())
        }
    }

    /// The registry is kiln's HarnessPort: it holds HARNESS-1, -2 and -4 now, and HARNESS-3 and -5
    /// fail only for want of seeing what reached an agent (CP-M2's AC-M2.2: "or will pass once
    /// its track lands the real one").
    #[test]
    fn kiln_s_registry_holds_the_harness_port_clauses_it_can_today_and_says_why_not_the_rest() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let cases = cases::<RegistryFixture>();
            // A hub of its own for each case.
            let hubs = futures::future::join_all(cases.iter().map(|_| crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true))).await;
            let mut hubs: std::collections::VecDeque<Arc<Hub>> = hubs.into_iter().collect::<anyhow::Result<_>>()?;
            // Their folders, removed once it's done (a scratch hub leaves its own behind).
            let folders: Vec<std::path::PathBuf> = hubs.iter().map(|h| h.cfg.server.data_dir.clone()).collect();
            let last = hubs.back().cloned().ok_or_else(|| anyhow::anyhow!("no cases"))?;
            let dir = tempfile::tempdir()?;
            let make = || {
                // There are as many hubs as cases: the last is never handed out twice.
                let hub = hubs.pop_front().unwrap_or_else(|| Arc::clone(&last));
                let cwd = dir.path().to_path_buf();
                async move { RegistryFixture { port: Named { hub, ids: Mutex::new(HashMap::new()) }, cwd } }
            };
            // Each case fails, named, if the registry hasn't answered it in 30 s (by kiln's runtime).
            let clock = crate::rt::clock();
            let report = kiln_seams::conformance::run_within("HarnessPort (kiln's registry)", &cases, make, || clock.sleep(Duration::from_secs(30))).await;
            drop(last);
            let tmp = std::env::temp_dir();
            for f in folders.iter().filter(|f| f.starts_with(&tmp) && *f != &tmp) {
                let _ = std::fs::remove_dir_all(f);
            }
            anyhow::ensure!(report.passed == HOLDS_TODAY, "held: {:?}, failed: {:?}", report.passed, report.failed);
            let failed: Vec<&str> = report.failed.iter().map(|(id, _)| *id).collect();
            anyhow::ensure!(failed == WAIT_FOR_T3, "failed: {:?}", report.failed);
            anyhow::ensure!(report.failed.iter().all(|(_, why)| why.ends_with("can't be seen here")), "failed otherwise: {:?}", report.failed);
            Ok(())
        })
    }
}
