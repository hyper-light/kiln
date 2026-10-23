//! Where repositories are (`docs/sync.md`, "Repositories on machines"): what each machine last
//! said it holds ([`Places`], kept for while it is away), and every repository kiln knows of as one
//! list ([`view`]): the ones it builds, the synced ones and the ones on machines, each once, with
//! the places it is.
//!
//! A repository is known by its **key**: its remote without scheme, user, credentials or `.git`
//! (`github.com/hyper-light/kiln`), so a clone over ssh and one over https are the same
//! repository. One with no remote is a place of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::server::View as SyncView;
use super::tasks::Task;
use super::wire::{Found, Repos};

/// A repository's key from its remote: `host/path`, lowercased, without scheme, user, port,
/// credentials, `.git` or slashes at either end. None for a remote that is a folder or file on one
/// machine (the same path elsewhere is another repository).
pub fn key_of(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            let host = host.split(':').next().unwrap_or(host);
            (host, path)
        }
        None => {
            // `[user@]host:path`, scp's form; a path with no host (`/srv/x.git`, `C:\x`) is local.
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') || authority.contains('\\') || authority.len() < 2 {
                return None;
            }
            (authority.rsplit_once('@').map_or(authority, |(_, h)| h), path)
        }
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

/// A repository's name as people say it: `owner/name` for a hosted one, the folder's name else.
pub fn name_of(key: &str, origin: Option<&str>, path: &str) -> String {
    if !key.starts_with("here:") {
        let rest = key.split_once('/').map_or(key, |(_, p)| p);
        // The remote's own spelling, when it has one.
        if let Some(p) = origin.and_then(|o| o.trim_end_matches('/').strip_suffix(".git").or(Some(o.trim_end_matches('/')))) {
            let tail: Vec<&str> = p.rsplit(['/', ':']).take(rest.split('/').count()).collect();
            let spelled: Vec<&str> = tail.into_iter().rev().collect();
            if spelled.join("/").eq_ignore_ascii_case(rest) {
                return spelled.join("/");
            }
        }
        return rest.to_owned();
    }
    path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

/// What a machine last said it holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Held {
    pub home: String,
    pub repos: Vec<Found>,
    pub error: Option<String>,
    /// Unix seconds.
    pub at: i64,
}

/// Every machine's repositories, as each last said (`<data>/sync/places.json`).
pub struct Places {
    file: PathBuf,
    held: Mutex<BTreeMap<String, Held>>,
}

impl Places {
    pub fn open(file: PathBuf) -> Self {
        let held = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { file, held: Mutex::new(held) }
    }

    /// What a machine said; true when it changed anything.
    pub fn heard(&self, machine: &str, r: Repos) -> bool {
        let held = Held { home: r.home, repos: r.repos, error: r.error, at: r.at };
        let changed = {
            let mut all = self.held.lock();
            let same = all.get(machine).is_some_and(|h| Held { at: held.at, ..h.clone() } == held);
            all.insert(machine.to_owned(), held);
            !same
        };
        if changed {
            self.save();
        }
        changed
    }

    /// Each machine's repositories that have a remote, by key (`crate::sync::share`).
    pub fn keys(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.held.lock().iter().map(|(m, h)| (m.clone(), h.repos.iter().filter_map(|f| f.origin.as_deref().and_then(key_of)).collect())).collect()
    }

    /// A machine kiln forgot.
    pub fn forget(&self, machine: &str) {
        if self.held.lock().remove(machine).is_some() {
            self.save();
        }
    }

    pub fn of(&self, machine: &str) -> Option<Held> {
        self.held.lock().get(machine).cloned()
    }

    pub fn all(&self) -> BTreeMap<String, Held> {
        self.held.lock().clone()
    }

    /// The repository at `path` on `machine`, as it last said.
    pub fn found(&self, machine: &str, path: &str) -> Option<Found> {
        self.held.lock().get(machine).and_then(|h| h.repos.iter().find(|f| f.path == path).cloned())
    }

    /// `~` and `~/…` on `machine` (where its user's home is, as it said), any whole path as it is;
    /// none when that is not a whole path, or `~` on a machine that hasn't said where home is.
    pub fn resolve(&self, machine: &str, path: &str) -> Option<String> {
        let p = path.trim();
        let home = self.of(machine).map(|h| h.home).filter(|h| !h.is_empty());
        let windows = home.as_deref().is_some_and(|h| h.contains('\\'));
        let out = if p == "~" {
            home?
        } else if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
            let home = home?;
            let sep = if windows { '\\' } else { '/' };
            let rest = if windows { rest.replace('/', "\\") } else { rest.to_owned() };
            format!("{}{sep}{rest}", home.trim_end_matches(['/', '\\']))
        } else {
            p.to_owned()
        };
        let whole = out.starts_with('/') || out.get(1..3) == Some(":\\") || out.get(1..3) == Some(":/");
        let plain = out.split(['/', '\\']).all(|s| s != "." && s != "..");
        (whole && plain && out.len() > 1).then(|| out.trim_end_matches(['/', '\\']).to_owned())
    }

    fn save(&self) {
        let bytes = serde_json::to_vec(&*self.held.lock()).unwrap_or_default();
        if let Err(e) = super::group::save_atomic(&self.file, &bytes) {
            tracing::warn!(error = %e, "what machines hold could not be saved");
        }
    }
}

// ------------------------------------------------------------------------------ the one list

/// A machine, as the list needs it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MachineView {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub labels: Vec<String>,
    pub connected: bool,
    /// Where `~` is on it, once it has said.
    pub home: Option<String>,
    /// When it last said what it holds (unix seconds).
    pub looked: Option<i64>,
    /// Why it can't say (no git), if it can't.
    pub error: Option<String>,
    /// Its runner clones, bundles and moves repositories (`CAP_REPOS`), and syncs (`CAP_SYNC`).
    pub can_clone: bool,
    pub can_sync: bool,
}

/// A synced copy, as a place shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PlaceCopy {
    /// The synced repo.
    pub repo: String,
    pub member: u64,
    pub state: &'static str,
    /// It syncs only for a move under way.
    pub moving: bool,
}

/// One place a repository is: a folder on a machine.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Place {
    pub machine: String,
    pub path: String,
    /// `copy` (kept in step by sync), `clone` (kiln cloned it there) or `found` (it was there).
    pub kind: &'static str,
    /// What git says of it, as the machine last looked.
    pub git: Option<Found>,
    pub copy: Option<PlaceCopy>,
    /// Its machine is connected.
    pub online: bool,
}

/// One repository and the places it is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Row {
    pub key: String,
    pub name: String,
    /// A remote to clone it from, when one is known.
    pub origin: Option<String>,
    /// The repository kiln builds, and the synced repo, that it is.
    pub built: Option<String>,
    pub synced: Option<String>,
    pub places: Vec<Place>,
}

/// Every repository kiln knows of, the machines and the tasks under way or done lately.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub repos: Vec<Row>,
    pub machines: Vec<MachineView>,
    pub tasks: Vec<Task>,
}

/// A machine as [`view`] takes it: its name, OS, architecture, labels, whether it is connected and
/// what its runner can do.
pub struct Machine {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub labels: Vec<String>,
    pub connected: bool,
    pub caps: u32,
}

/// A repository kiln builds, as [`view`] takes it.
pub struct Built {
    pub id: String,
    pub name: String,
    pub clone_url: String,
}

/// The one list: each repository kiln builds; each synced repo, with the repository it is (same
/// name as one kiln builds, else the remote its copies have); each repository a machine holds, by
/// its key. Places are each machine's folder once: a synced copy that is a git repository is one
/// place, with both.
pub fn view(built: &[Built], synced: &SyncView, machines: &[Machine], held: &BTreeMap<String, Held>, tasks: Vec<Task>) -> View {
    let online: HashMap<&str, bool> = machines.iter().map(|m| (m.name.as_str(), m.connected)).collect();
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for b in built {
        let key = key_of(&b.clone_url).unwrap_or_else(|| format!("built:{}", b.id));
        let row = rows.entry(key.clone()).or_insert_with(|| Row { key, name: b.name.clone(), ..Row::default() });
        row.built = Some(b.id.clone());
        row.origin = Some(b.clone_url.clone());
    }
    // What git says of each folder, by machine and path.
    let mut git: HashMap<(&str, &str), &Found> = HashMap::new();
    for (machine, h) in held {
        for f in &h.repos {
            git.insert((machine.as_str(), f.path.as_str()), f);
        }
    }
    let mut placed: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for s in &synced.repos {
        let by_name = built.iter().find(|b| {
            let n = b.name.to_lowercase();
            let s = s.name.to_lowercase();
            n == s || n.rsplit('/').next() == Some(s.as_str())
        });
        let by_origin = s.copies.iter().filter_map(|c| git.get(&(c.machine.as_str(), c.path.as_str()))).find_map(|f| f.origin.as_deref().and_then(key_of));
        let key = by_name.and_then(|b| key_of(&b.clone_url).or_else(|| Some(format!("built:{}", b.id)))).or(by_origin).unwrap_or_else(|| format!("synced:{}", s.id));
        let row = rows.entry(key.clone()).or_insert_with(|| Row { key, name: s.name.clone(), ..Row::default() });
        if s.transient.is_none() {
            row.synced = Some(s.id.clone());
        }
        for c in &s.copies {
            let found = git.get(&(c.machine.as_str(), c.path.as_str())).map(|f| (*f).clone());
            if row.origin.is_none() {
                row.origin = found.as_ref().and_then(|f| f.origin.clone());
            }
            placed.insert((c.machine.clone(), c.path.clone()));
            row.places.push(Place {
                machine: c.machine.clone(),
                path: c.path.clone(),
                kind: "copy",
                git: found,
                copy: Some(PlaceCopy { repo: s.id.clone(), member: c.member, state: c.state, moving: s.transient.is_some() }),
                online: online.get(c.machine.as_str()).copied().unwrap_or(false),
            });
        }
    }
    for (machine, h) in held {
        for f in &h.repos {
            if placed.contains(&(machine.clone(), f.path.clone())) {
                continue;
            }
            let key = f.origin.as_deref().and_then(key_of).unwrap_or_else(|| format!("here:{machine}:{}", f.path));
            let row = rows.entry(key.clone()).or_insert_with(|| Row { name: name_of(&key, f.origin.as_deref(), &f.path), key, ..Row::default() });
            if row.origin.is_none() {
                row.origin = f.origin.clone();
            }
            row.places.push(Place {
                machine: machine.clone(),
                path: f.path.clone(),
                kind: if f.kiln { "clone" } else { "found" },
                git: Some(f.clone()),
                copy: None,
                online: online.get(machine.as_str()).copied().unwrap_or(false),
            });
        }
    }
    let mut repos: Vec<Row> = rows.into_values().collect();
    for r in &mut repos {
        r.places.sort_by(|a, b| a.machine.cmp(&b.machine).then_with(|| a.path.cmp(&b.path)));
    }
    repos.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.key.cmp(&b.key)));
    let machines = machines
        .iter()
        .map(|m| {
            let h = held.get(&m.name);
            MachineView {
                name: m.name.clone(),
                os: m.os.clone(),
                arch: m.arch.clone(),
                labels: m.labels.clone(),
                connected: m.connected,
                home: h.map(|h| h.home.clone()).filter(|h| !h.is_empty()),
                looked: h.map(|h| h.at),
                error: h.and_then(|h| h.error.clone()),
                can_clone: m.connected && m.caps & crate::proto::CAP_REPOS != 0,
                can_sync: m.connected && m.caps & crate::proto::CAP_SYNC != 0,
            }
        })
        .collect();
    View { repos, machines, tasks }
}

/// The list as it is now: the repositories kiln builds, the synced ones, the machines (not job
/// pods) and what each holds, and the tasks.
pub async fn now(hub: &crate::hub::Hub) -> anyhow::Result<View> {
    let built: Vec<Built> = hub.store.repos().await?.into_iter().map(|r| Built { id: r.id, name: r.name, clone_url: r.clone_url }).collect();
    let machines: Vec<Machine> = hub
        .agent_list()
        .await
        .into_iter()
        .filter(|a| !a.ephemeral)
        .map(|a| Machine { caps: hub.runner_caps(&a.name), name: a.name, os: a.os, arch: a.arch, labels: a.labels, connected: a.connected })
        .collect();
    Ok(view(&built, &hub.sync.view(), &machines, &hub.places.all(), hub.tasks.list()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::server::{CopyView, RepoView};

    #[test]
    fn a_remote_s_key_is_the_same_however_it_is_reached() {
        for url in [
            "https://github.com/hyper-light/kiln.git",
            "https://github.com/hyper-light/kiln",
            "https://x-access-token:secret@github.com/hyper-light/kiln.git",
            "git@github.com:hyper-light/kiln.git",
            "ssh://git@github.com/hyper-light/kiln",
            "ssh://git@github.com:22/hyper-light/kiln.git/",
            "HTTPS://GitHub.com/Hyper-Light/Kiln",
        ] {
            assert_eq!(key_of(url).as_deref(), Some("github.com/hyper-light/kiln"), "{url}");
        }
        assert_eq!(key_of("https://gitlab.example.com/group/sub/proj.git").as_deref(), Some("gitlab.example.com/group/sub/proj"));
        for local in ["/srv/git/kiln.git", "file:///srv/git/kiln.git", "C:\\src\\kiln", "../kiln", "", "https://github.com"] {
            assert_eq!(key_of(local), None, "{local}");
        }
    }

    #[test]
    fn names_are_said_as_their_remote_spells_them() {
        assert_eq!(name_of("github.com/hyper-light/kiln", Some("git@github.com:Hyper-Light/Kiln.git"), "/x"), "Hyper-Light/Kiln");
        assert_eq!(name_of("github.com/a/b", None, "/x"), "a/b");
        assert_eq!(name_of("here:mac:/Users/a/notes", None, "/Users/a/notes"), "notes");
    }

    #[test]
    fn a_path_resolves_on_its_own_machine() {
        let dir = std::env::temp_dir().join(format!("kiln-places-{}", crate::util::new_id()));
        let p = Places::open(dir.join("places.json"));
        p.heard("mac", Repos { home: "/Users/ada".into(), ..Repos::default() });
        p.heard("win", Repos { home: "C:\\Users\\ada".into(), ..Repos::default() });
        assert_eq!(p.resolve("mac", "~/Projects/kiln").as_deref(), Some("/Users/ada/Projects/kiln"));
        assert_eq!(p.resolve("win", "~/Projects/kiln").as_deref(), Some("C:\\Users\\ada\\Projects\\kiln"));
        assert_eq!(p.resolve("mac", "/opt/kiln/").as_deref(), Some("/opt/kiln"));
        assert_eq!(p.resolve("box", "~/x"), None, "a machine that hasn't said where home is");
        assert_eq!(p.resolve("mac", "Projects/kiln"), None, "a whole path or ~");
        assert_eq!(p.resolve("mac", "/a/../etc"), None);
        assert_eq!(p.resolve("mac", "/"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn found(path: &str, origin: Option<&str>, kiln: bool) -> Found {
        Found { path: path.into(), origin: origin.map(str::to_owned), branch: Some("main".into()), kiln, ..Found::default() }
    }

    #[test]
    fn every_repository_is_one_row_with_every_place_it_is() {
        let built = [Built { id: "b1".into(), name: "hyper-light/kiln".into(), clone_url: "https://github.com/hyper-light/kiln.git".into() }];
        let mut held = BTreeMap::new();
        held.insert(
            "mac".to_owned(),
            Held {
                home: "/Users/ada".into(),
                repos: vec![found("/Users/ada/Projects/kiln", Some("git@github.com:hyper-light/kiln.git"), false), found("/Users/ada/notes", None, false)],
                ..Held::default()
            },
        );
        held.insert("box".to_owned(), Held { home: "/home/ada".into(), repos: vec![found("/home/ada/src/kiln", Some("https://github.com/hyper-light/kiln"), true)], ..Held::default() });
        // A synced folder named for the repository kiln builds, its copy on the Mac the Mac's clone.
        let synced = SyncView {
            repos: vec![RepoView {
                id: "s1".into(),
                name: "kiln".into(),
                copies: vec![CopyView { member: 2, machine: "mac".into(), path: "/Users/ada/Projects/kiln".into(), state: "in step", ..CopyView::default() }],
                ..RepoView::default()
            }],
            error: None,
        };
        let machines = [
            Machine { name: "mac".into(), os: "macos".into(), arch: "aarch64".into(), labels: vec![], connected: true, caps: crate::proto::CAP_SYNC | crate::proto::CAP_REPOS },
            Machine { name: "box".into(), os: "linux".into(), arch: "x86_64".into(), labels: vec![], connected: false, caps: 0 },
        ];
        let v = view(&built, &synced, &machines, &held, Vec::new());
        assert_eq!(v.repos.len(), 2, "{:?}", v.repos.iter().map(|r| &r.key).collect::<Vec<_>>());
        let kiln = v.repos.iter().find(|r| r.key == "github.com/hyper-light/kiln");
        let kiln = kiln.map(|r| (r.built.as_deref(), r.synced.as_deref(), r.places.iter().map(|p| (p.machine.as_str(), p.kind, p.git.is_some(), p.online)).collect::<Vec<_>>()));
        assert_eq!(kiln, Some((Some("b1"), Some("s1"), vec![("box", "clone", true, false), ("mac", "copy", true, true)])));
        let notes = v.repos.iter().find(|r| r.name == "notes").map(|r| (r.key.clone(), r.built.clone(), r.places.len()));
        assert_eq!(notes, Some(("here:mac:/Users/ada/notes".to_owned(), None, 1)));
        let mac = v.machines.iter().find(|m| m.name == "mac").map(|m| (m.can_clone, m.home.clone()));
        assert_eq!(mac, Some((true, Some("/Users/ada".to_owned()))));
        assert!(v.machines.iter().any(|m| m.name == "box" && !m.can_clone), "an offline machine clones nothing");
    }
}
