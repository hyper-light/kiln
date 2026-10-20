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
