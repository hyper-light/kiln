//! kiln's runners, faked for tests: runners by name with their labels and OS, runs that start
//! queued and pass a few reads later (or fail, when what they run says `broken`), every run kept
//! with what it ran. Artifacts come from memory, as the other fakes' do.

use std::pin::Pin;

use anyhow::Result;
use kiln_cd::providers::host::Host;
use parking_lot::Mutex;
use serde_json::Value;

use super::fake::MemFiles;
use super::runner::{BoxFut, RunState, Runners, Work};

/// A run as the fake keeps it: the app and commit it's of, what it ran, and how it stands.
#[derive(Clone, Debug, Default)]
pub struct Run {
    pub id: String,
    pub app: String,
    pub revision: String,
    /// A written workflow's YAML, or `workflow#job inputs` for one of the repository's.
    pub ran: String,
    pub status: String,
    /// Reads left before it ends.
    pub busy: u32,
}

#[derive(Default)]
pub struct FakeRunners {
    pub files: MemFiles,
    /// Runners: name, labels, OS.
    pub hosts: Vec<(String, Vec<String>, String)>,
    pub runs: Mutex<Vec<Run>>,
    /// Environments defined: app, name, kind, selector.
    pub environments: Mutex<Vec<(String, String, String, String)>>,
    /// Deployments taken down: kind, environment, hosts.
    pub undeployed: Mutex<Vec<(String, String, Vec<String>)>>,
}

impl FakeRunners {
    pub fn with_hosts(hosts: &[(&str, &[&str], &str)]) -> FakeRunners {
        FakeRunners { hosts: hosts.iter().map(|(n, l, os)| ((*n).to_owned(), l.iter().map(|s| (*s).to_owned()).collect(), (*os).to_owned())).collect(), ..FakeRunners::default() }
    }

    /// The newest run's.
    pub fn last(&self) -> Run {
        self.runs.lock().last().cloned().unwrap_or_default()
    }
}

impl crate::cd::artifacts::Files for FakeRunners {
    fn open<'a>(&'a self, hex: &'a str) -> Pin<Box<dyn std::future::Future<Output = Result<(async_channel::Receiver<bytes::Bytes>, u64)>> + Send + 'a>> {
        self.files.open(hex)
    }

    fn secret<'a>(&'a self, app: &'a str, reference: &'a str) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        self.files.secret(app, reference)
    }

    fn runners(&self) -> Option<&dyn Runners> {
        Some(self)
    }
}

impl Runners for FakeRunners {
    fn pick<'a>(&'a self, selector: &'a str) -> BoxFut<'a, Result<Vec<Host>>> {
        Box::pin(async move {
            // The fake reads `label <name>` alone, and `&&`s of them.
            let wanted: Vec<&str> = selector.split("&&").map(str::trim).filter_map(|c| c.strip_prefix("label ")).map(str::trim).collect();
            Ok(self.hosts.iter().filter(|(_, labels, _)| wanted.iter().all(|w| labels.iter().any(|l| l == w))).map(|(n, _, os)| Host { name: n.clone(), os: os.clone() }).collect())
        })
    }

    fn start<'a>(&'a self, app: &'a str, revision: &'a str, work: Work<'a>) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            let ran = match work {
                Work::Written { yaml, .. } => yaml.to_owned(),
                Work::Own { workflow, job, inputs } => format!("{workflow}#{} {}", job.unwrap_or("*"), Value::Object(inputs.clone())),
            };
            let mut runs = self.runs.lock();
            let id = format!("run-{}", runs.len() + 1);
            runs.push(Run { id: id.clone(), app: app.to_owned(), revision: revision.to_owned(), ran, status: "queued".into(), busy: 2 });
            Ok(id)
        })
    }

    fn run<'a>(&'a self, id: &'a str) -> BoxFut<'a, Result<RunState>> {
        Box::pin(async move {
            let mut runs = self.runs.lock();
            let (i, r) = runs.iter_mut().enumerate().find(|(_, r)| r.id == id).ok_or_else(|| anyhow::anyhow!("no run {id}"))?;
            if r.busy > 0 {
                r.busy -= 1;
                r.status = match r.busy {
                    0 if r.ran.contains("broken") => "failed".into(),
                    0 => "succeeded".into(),
                    _ => "running".into(),
                };
            }
            let error = (r.status == "failed").then(|| "the step's script exited with 1".to_owned());
            Ok(RunState { id: r.id.clone(), number: i as i64 + 1, status: r.status.clone(), error, url: format!("https://kiln.test/runs/{}", r.id) })
        })
    }

    fn link(&self, hex: &str, secs: i64) -> Result<String> {
        Ok(format!("https://kiln.test/_kiln/blob/object-{hex}?se={secs}&sp=r&sig=fake"))
    }

    fn environment<'a>(&'a self, app: &'a str, name: &'a str, kind: &'a str, selector: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let mut envs = self.environments.lock();
            let e = (app.to_owned(), name.to_owned(), kind.to_owned(), selector.to_owned());
            if !envs.contains(&e) {
                envs.push(e);
            }
            Ok(())
        })
    }

    fn undeploy<'a>(&'a self, _app: &'a str, kind: &'a str, name: &'a str, hosts: &'a [String]) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            self.undeployed.lock().push((kind.to_owned(), name.to_owned(), hosts.to_vec()));
            Ok(())
        })
    }
}
