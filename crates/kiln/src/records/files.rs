//! kiln's own JSON state files carried into Records and compared (CP-T1.5; kiln goal 0d717247): the files kiln
//! writes and reads back itself, as docs/records-inventory.md's JSON files table says ("Records `kiln.files`").
//! Formats written for other programs (an SBOM, a signature bundle, a VM's or a jail's configuration, a harness's
//! own settings, Docker's, GitHub's), lab and bench output, scratch for one leg or push, and caches rebuilt when
//! they're gone stay files: the inventory says which, file by file.
//!
//! Each file is a record of the family [`FAMILY`], keyed by its machine, its base folder and its path under it
//! (`adacachy:state/reach/told.json`), its value the file's bytes as they are: [`compare`] tells a file changed by a
//! byte. [`FILES`] says where each kind is kept, and which code writes and reads it. As with kiln.db's tables, the
//! copy is taken with its writers stopped, and [`compare`] says what moved meanwhile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kiln_seams::records::{Consistency, KEY_MAX, Records, Refusal, SCAN_MAX, Scan, TX_MAX, Tx, VALUE_MAX, Write};

use super::carry::{Difference, Differs};

/// The family every state file is kept in.
pub const FAMILY: &str = "kiln.files";

/// The folders kiln keeps its state files under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Base {
    /// The server's data folder (`[server] data_dir`, `Config::data`).
    Data,
    /// A runner's state folder (`--state-dir`; `config::default_data_dir()/agent`; the server's own runner's is
    /// `<data>/local-agent`). A blue/green instance's own files are under its `instances/<id>`.
    State,
    /// A runner's work folder (`--work-dir`; `<state>/work`).
    Work,
    /// `kiln run`'s copies (`~/.kiln/reach`, `reach::node::Config::for_runner`).
    Reach,
    /// The agents' skills (`skills::place::root`).
    Skills,
}

impl Base {
    /// The base's name in a record's key.
    pub fn name(self) -> &'static str {
        match self {
            Base::Data => "data",
            Base::State => "state",
            Base::Work => "work",
            Base::Reach => "reach",
            Base::Skills => "skills",
        }
    }
}

/// One kind of state file: the names the inventory lists it by, where it's kept (paths under its base, each
/// segment a name or a pattern where `*` stands for any part of one), and the code that writes and reads it.
#[derive(Debug, Clone, Copy)]
pub struct StateFile {
    pub names: &'static [&'static str],
    pub base: Base,
    pub paths: &'static [&'static str],
    pub writer: &'static str,
    pub reader: &'static str,
}

const fn file(names: &'static [&'static str], base: Base, paths: &'static [&'static str], writer: &'static str, reader: &'static str) -> StateFile {
    StateFile { names, base, paths, writer, reader }
}

/// Every kind of kiln's own JSON state file.
pub const FILES: &[StateFile] = &[
    // The server's data folder.
    file(&["{}.json"], Base::Data, &["harness/files/*.json"], "harness::kept::Keeper::save", "harness::kept::Keeper::list"),
    file(&["{id}.json"], Base::Data, &["harness/visuals/*.json"], "harness::visuals::Visuals::save", "harness::visuals::Visuals::get"),
    file(&["tasks.json"], Base::Data, &["sync/tasks.json"], "sync::tasks::Tasks::save", "sync::tasks::Tasks::open"),
    file(&["builds.json"], Base::Data, &["binaries/builds.json"], "fleet::remember", "fleet::load_builds"),
    file(&["search-meta.json"], Base::Data, &["cache/search-meta.json"], "ci::search::with_meta", "ci::search::with_meta"),
    file(&["clouds.json"], Base::Data, &["observe/clouds.json"], "observe::cloud::Clouds::save", "observe::cloud::Clouds::open"),
    file(&["eventhubs-{name}.json"], Base::Data, &["observe/eventhubs-*.json"], "observe::cloud::eventhubs::Kept::save", "observe::cloud::eventhubs::load"),
    file(&["indexes.json"], Base::Data, &["observe/indexes.json"], "observe::indexes::Indexes::save", "observe::indexes::Indexes::open"),
    file(&["next.json", "{public_id}.json"], Base::Data, &["observe/incidents/*.json"], "observe::incidents::Incidents::write, ::forget", "observe::incidents::Incidents::open_bounded"),
    file(&["members.json"], Base::Data, &["observe/members.json"], "observe::owner::Owner::save_members", "observe::owner::Owner::new"),
    file(&["rehydrations.json"], Base::Data, &["observe/rehydrations.json"], "observe::rehydrate::Rehydrations::save", "observe::rehydrate::Rehydrations::open"),
    file(&["scrapes.json"], Base::Data, &["observe/scrapes.json"], "observe::scrape::Targets::save", "observe::scrape::Targets::open"),
    file(&["supervise-{}.json"], Base::Data, &["supervise-*.json"], "supervise::handle", "supervise::run"),
    file(&["names.json"], Base::Data, &["harness/names.json"], "harness::registry::Registry (keep_json)", "harness::registry::Registry::with"),
    file(&["epochs.json"], Base::Data, &["harness/epochs.json"], "harness::registry::Registry (keep_json)", "harness::registry::Registry::with"),
    file(&["keep-off.json"], Base::Data, &["harness/keep-off.json"], "harness::registry::Registry (keep_json)", "harness::registry::Registry::with"),
    file(&["holders.json"], Base::Data, &["harness/holders.json"], "harness::registry::Registry::save_holders", "harness::registry::Registry::with"),
    file(&["mirrors.json"], Base::Data, &["harness/mirrors.json"], "convo::mirror::Mirrors::save", "convo::mirror::Mirrors::start"),
    file(&["local.json"], Base::Data, &["harness/local.json"], "harness::host::Host::save_checked (the server's own host)", "harness::host::Host::keeping_in"),
    file(&["{id}.json"], Base::Data, &["harness/archived/*.json"], "harness::host::Host::archive_mode (the server's own host)", "harness::host::Host::restore_archive_mode"),
    file(&["moved-git.json"], Base::Data, &["harness/attachments/*/moved-git.json"], "harness::host::Host::take_over_now (the server's own host)", "harness::session::git_in"),
    file(&["{name}.json"], Base::Data, &["harness/homes/*/*.ran-in/*.json"], "harness::isolated::home::keep_ran_in (the server's own host)", "harness::isolated::home::ran_in"),
    file(&["harness-models.json"], Base::Data, &["harness/harness-models.json"], "harness::kinds (the models its harnesses list)", "harness::kinds::keep_models_in"),
    file(&["harness-models.json"], Base::State, &["harness-models.json", "instances/*/harness-models.json"], "harness::kinds (the models its harnesses list)", "harness::kinds::keep_models_in"),
    // A runner's state folder.
    file(&["terminal-handover.json"], Base::State, &["terminal-handover.json", "instances/*/terminal-handover.json"], "agent::write_handover", "agent::adopt_handover"),
    file(&["{key}.json"], Base::State, &["reach/trees/*.json"], "reach::node (a synced tree's hashes, `hashes_of`)", "reach::sync::Hashes"),
    file(&["supervise-{}.json"], Base::State, &["supervise-*.json"], "supervise::handle", "supervise::run"),
    file(&["agent-limit.json"], Base::State, &["agent-limit.json"], "harness::limit::keep", "harness::limit::kept"),
    file(&["runner-engine.json"], Base::State, &["runner-engine.json"], "harness::engine::keep", "harness::engine::kept"),
    file(&["told.json"], Base::State, &["reach/told.json"], "reach::told::persist", "reach::told::read"),
    file(&["pins.json"], Base::State, &["reach/pins.json"], "reach::e2e::Pins::save", "reach::e2e::Pins::open"),
    file(&["pace.json"], Base::State, &["reach/pace.json"], "sense::pace::Keeper::save", "sense::pace::start"),
    file(&["{}-{}.json"], Base::State, &["headroom/paced/*-*.json"], "headroom::pace::tell", "headroom::pace::release_ended, ::guard"),
    file(&["instance.json"], Base::State, &["instances/*/instance.json"], "upgrade::install_alongside", "upgrade::created_at"),
    file(&["replaces.json"], Base::State, &["instances/*/replaces.json"], "upgrade::install_alongside, ::note_retired", "upgrade::duties, ::replacing"),
    file(&["copies.json"], Base::State, &["sync/copies.json"], "sync::runner::Owner::save_registry", "sync::runner::Owner::new"),
    file(&["{safe}-{}.json"], Base::State, &["sync/copies/*-*.json"], "sync::group::Feed::save", "sync::group::Feed::open"),
    file(&["member.json"], Base::State, &["observe/member.json"], "observe::owner::Owner::heard_from_server", "observe::owner::Owner::new"),
    file(&["harness.json"], Base::State, &["harness.json", "instances/*/harness.json"], "harness::host::Host::save_checked", "harness::host::Host::restore_from"),
    file(&["agent-handover.json"], Base::State, &["agent-handover.json", "instances/*/agent-handover.json"], "harness::host::Host::hand_over_all_inner", "harness::host::Host::take_handover"),
    file(&["handovers-taken.json"], Base::State, &["handovers-taken.json"], "harness::host::Handovers::write_held", "harness::host::Handovers::read"),
    file(&["handovers-decided.json"], Base::State, &["handovers-decided.json"], "harness::host::Locked::decide", "harness::host::Locked::decisions"),
    file(&["{id}.json"], Base::State, &["archived/*.json"], "harness::host::Host::archive_mode", "harness::host::Host::restore_archive_mode"),
    file(&["moved-git.json"], Base::State, &["attachments/*/moved-git.json", "instances/*/attachments/*/moved-git.json"], "harness::host::Host::take_over_now", "harness::session::git_in"),
    file(&["{name}.json"], Base::State, &["homes/*/*.ran-in/*.json"], "harness::isolated::home::keep_ran_in", "harness::isolated::home::ran_in"),
    // A runner's work folder.
    file(&["lane.json"], Base::Work, &["warm/*/*/lane.json"], "runner::warm::lease_with", "runner::warm::read_lane"),
    file(&["runs.json"], Base::Work, &["warm/*/*/runs.json"], "runner::warm::ran", "runner::warm::read_runs"),
    file(&["{n}.json"], Base::Work, &["warm/*/*/*.json"], "runner::warm::write_slot", "runner::warm::read_slots"),
    file(&["{project}.json", "{}.json"], Base::Work, &["_firecracker/deployments/*.json"], "runner::firecracker::deploy", "runner::firecracker::restore"),
    // `kiln run`'s copies, and the agents' skills.
    file(&[".keep.json"], Base::Reach, &[".keep.json"], "reach::keeper::Keeper::save", "reach::keeper::Keeper::hold"),
    file(&["{owner}.json", "{}.json"], Base::Skills, &["owners/*.json"], "skills::place::Place::apply", "skills::place::Place::held"),
];

/// Whether `name` is `pattern`'s, where `*` stands for any part of it (none too). Read left to right, each `*`
/// taken as short as it can be and grown only when what follows it doesn't match: no recursion.
fn matches(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut i, mut j) = (0, 0);
    // The last `*` seen, and where in `name` it was taken to end.
    let mut star: Option<(usize, usize)> = None;
    while j < n.len() {
        match p.get(i) {
            Some(b'*') => {
                star = Some((i, j));
                i += 1;
            }
            Some(c) if Some(c) == n.get(j) => {
                i += 1;
                j += 1;
            }
            _ => match star {
                Some((si, sj)) => {
                    i = si + 1;
                    j = sj + 1;
                    star = Some((si, sj + 1));
                }
                None => return false,
            },
        }
    }
    p.get(i..).is_some_and(|rest| rest.iter().all(|c| *c == b'*'))
}

/// The files under `root` that `path` names, each with its path under `root` (forward slashes), segment by
/// segment: a folder or file that isn't there is none.
fn found_at(root: &Path, path: &str) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let segments: Vec<&str> = path.split('/').collect();
    let mut at: Vec<(String, PathBuf)> = vec![(String::new(), root.to_path_buf())];
    for (k, seg) in segments.iter().enumerate() {
        let last = k + 1 == segments.len();
        let mut next = Vec::new();
        for (rel, dir) in at {
            let join = |name: &str| if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
            if !seg.contains('*') {
                let p = dir.join(seg);
                if (last && p.is_file()) || (!last && p.is_dir()) {
                    next.push((join(seg), p));
                }
                continue;
            }
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(anyhow::anyhow!("{}: {e}", dir.display())),
            };
            for entry in entries {
                let entry = entry.map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let p = entry.path();
                if matches(seg, &name) && ((last && p.is_file()) || (!last && p.is_dir())) {
                    next.push((join(&name), p));
                }
            }
        }
        at = next;
    }
    Ok(at)
}

/// Where this machine's state files are: each base's folder (a base not given has none here), and the machine's
/// name, which every key starts with.
#[derive(Debug, Clone)]
pub struct Roots {
    pub machine: String,
    pub bases: BTreeMap<Base, PathBuf>,
}

/// Every state file [`FILES`] names under `roots`, by its record's key, each once.
pub fn found(roots: &Roots) -> anyhow::Result<BTreeMap<Vec<u8>, PathBuf>> {
    let mut out = BTreeMap::new();
    for f in FILES {
        let Some(root) = roots.bases.get(&f.base) else { continue };
        for path in f.paths {
            for (rel, p) in found_at(root, path)? {
                let key = format!("{}:{}/{rel}", roots.machine, f.base.name()).into_bytes();
                anyhow::ensure!(key.len() <= KEY_MAX, "{} is {} bytes as a key, over the Records seam's {KEY_MAX}", p.display(), key.len());
                out.insert(key, p);
            }
        }
    }
    Ok(out)
}

fn refused(r: Refusal) -> anyhow::Error {
    anyhow::anyhow!("records: {r}")
}

/// Every state file under `roots` put into [`FAMILY`] as it is, a commit's most at a time. How many it put.
pub async fn carry(roots: &Roots, records: &dyn Records) -> anyhow::Result<usize> {
    let all: Vec<(Vec<u8>, PathBuf)> = found(roots)?.into_iter().collect();
    for page in all.chunks(TX_MAX) {
        let mut writes = Vec::with_capacity(page.len());
        for (key, path) in page {
            let value = std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            // A file the seam can't hold as one record is named, not lost in a refused commit.
            anyhow::ensure!(value.len() <= VALUE_MAX, "{} is {} bytes, over the Records seam's {VALUE_MAX}", path.display(), value.len());
            writes.push(Write::Put { family: FAMILY.to_owned(), key: key.clone(), value });
        }
        records.commit(Tx { conds: Vec::new(), writes }).await.map_err(|e| anyhow::anyhow!("carrying kiln's state files: {}", refused(e)))?;
    }
    Ok(all.len())
}

/// Each state file under `roots` against its record: a file with none ([`Differs::NoRecord`]), a record of this
/// machine with no file ([`Differs::NoRow`]), or bytes that differ ([`Differs::Value`]: the file's length, the
/// record's). [`Difference::table`] is the file's path, its key the record's.
pub async fn compare(roots: &Roots, records: &dyn Records) -> anyhow::Result<Vec<Difference>> {
    let at = Consistency::Snapshot(records.head().await.map_err(refused)?);
    let prefix = format!("{}:", roots.machine).into_bytes();
    let mut kept: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut after = None;
    loop {
        let p = records.scan(FAMILY, Scan { prefix: prefix.clone(), after: after.take(), limit: SCAN_MAX, at }).await.map_err(refused)?;
        after = p.rows.last().map(|r| r.key.clone());
        kept.extend(p.rows.into_iter().map(|r| (r.key, r.value)));
        if !p.more || after.is_none() {
            break;
        }
    }
    let mut out = Vec::new();
    for (key, path) in found(roots)? {
        let have = std::fs::read(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let differs = match kept.remove(&key) {
            None => Some(Differs::NoRecord),
            Some(v) if v != have => Some(Differs::Value { from: have.len(), to: v.len() }),
            Some(_) => None,
        };
        if let Some(differs) = differs {
            out.push(Difference { table: path.display().to_string(), key: String::from_utf8_lossy(&key).into_owned(), differs });
        }
    }
    out.extend(kept.into_keys().map(|key| Difference { table: FAMILY.to_owned(), key: String::from_utf8_lossy(&key).into_owned(), differs: Differs::NoRow }));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use kiln_seams::records::SimRecords;

    use super::*;

    #[test]
    fn a_pattern_stands_for_any_part_of_a_name() {
        for (p, n, want) in [
            ("*.json", "told.json", true),
            ("*.json", "told.jsonl", false),
            ("*-*.json", "123-456.json", true),
            ("*-*.json", "123.json", false),
            ("supervise-*.json", "supervise-ab.json", true),
            ("*.ran-in", "claude.ran-in", true),
            ("told.json", "told.json", true),
            ("*", "", true),
            ("a*b*c", "aXbYbZc", true),
            ("a*b*c", "aXbYbZ", false),
        ] {
            assert_eq!(matches(p, n), want, "{p} against {n}");
        }
    }

    /// A machine with a state file of each base and a templated one or two, besides files kiln keeps no state in.
    fn machine(root: &Path) -> anyhow::Result<Roots> {
        let put = |rel: &str, body: &str| -> anyhow::Result<()> {
            let p = root.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            Ok(std::fs::write(p, body)?)
        };
        put("data/sync/tasks.json", "{\"tasks\":[]}")?;
        put("data/observe/incidents/next.json", "7")?;
        put("data/observe/incidents/inc-3.json", "{\"id\":\"inc-3\"}")?;
        put("state/reach/told.json", "{\"told\":[1,2]}")?;
        put("state/reach/pins.json", "{\"pins\":{}}")?;
        put("state/instances/b2/harness.json", "{\"sessions\":[]}")?;
        put("state/homes/ada/claude.ran-in/ab12.json", "\"/work\"")?;
        put("work/warm/r1/l1/lane.json", "{}")?;
        put("work/warm/r1/l1/3.json", "{\"slot\":3}")?;
        // Not kiln's state: a leg's SBOM, and a file in a state folder no kind names.
        put("work/legs/l1/sbom.cdx.json", "{}")?;
        put("state/reach/notes.txt", "x")?;
        let bases = [(Base::Data, "data"), (Base::State, "state"), (Base::Work, "work")].into_iter().map(|(b, d)| (b, root.join(d))).collect();
        Ok(Roots { machine: "m1".into(), bases })
    }

    /// CP-T1.5 (goal 0d717247): every state file of each base carried into Records reads back the same, byte for
    /// byte, under its machine, base and path, and nothing else is carried; a file changed, one added and one
    /// deleted after the copy are each found by its key; carrying again leaves only the deleted one's record.
    #[test]
    fn cp_t1_5_kilns_state_files_carried_into_records_compare_byte_by_byte() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let roots = machine(dir.path())?;
            let keys: Vec<String> = found(&roots)?.into_keys().map(|k| String::from_utf8_lossy(&k).into_owned()).collect();
            let want = [
                "m1:data/observe/incidents/inc-3.json",
                "m1:data/observe/incidents/next.json",
                "m1:data/sync/tasks.json",
                "m1:state/homes/ada/claude.ran-in/ab12.json",
                "m1:state/instances/b2/harness.json",
                "m1:state/reach/pins.json",
                "m1:state/reach/told.json",
                "m1:work/warm/r1/l1/3.json",
                "m1:work/warm/r1/l1/lane.json",
            ];
            anyhow::ensure!(keys == want, "{keys:?}");
            let records = SimRecords::new();
            anyhow::ensure!(carry(&roots, &records).await? == want.len(), "each once");
            let same = compare(&roots, &records).await?;
            anyhow::ensure!(same.is_empty(), "{same:?}");

            let root = dir.path();
            std::fs::write(root.join("state/reach/told.json"), "{\"told\":[1,2,3]}")?;
            std::fs::write(root.join("data/observe/incidents/inc-4.json"), "{}")?;
            std::fs::remove_file(root.join("state/reach/pins.json"))?;
            let mut found: Vec<(String, Differs)> = compare(&roots, &records).await?.into_iter().map(|d| (d.key, d.differs)).collect();
            found.sort_by(|a, b| a.0.cmp(&b.0));
            let want = vec![
                ("m1:data/observe/incidents/inc-4.json".to_owned(), Differs::NoRecord),
                ("m1:state/reach/pins.json".to_owned(), Differs::NoRow),
                ("m1:state/reach/told.json".to_owned(), Differs::Value { from: 16, to: 14 }),
            ];
            anyhow::ensure!(found == want, "{found:?}");
            carry(&roots, &records).await?;
            let left: Vec<(String, Differs)> = compare(&roots, &records).await?.into_iter().map(|d| (d.key, d.differs)).collect();
            anyhow::ensure!(left == vec![("m1:state/reach/pins.json".to_owned(), Differs::NoRow)], "a copy puts, it doesn't delete: {left:?}");

            // Another machine's records are its own: this one's compare doesn't read them.
            let other = Roots { machine: "m2".into(), bases: roots.bases.clone() };
            carry(&other, &records).await?;
            anyhow::ensure!(compare(&roots, &records).await?.len() == 1, "m2's records aren't m1's");
            Ok(())
        })
    }

    /// A state file too big for one record fails the copy by its path.
    #[test]
    fn cp_t1_5_a_state_file_over_a_records_value_is_named() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let p = dir.path().join("sync");
            std::fs::create_dir_all(&p)?;
            std::fs::write(p.join("tasks.json"), vec![b' '; VALUE_MAX + 1])?;
            let roots = Roots { machine: "m1".into(), bases: [(Base::Data, dir.path().to_path_buf())].into_iter().collect() };
            let refused = match carry(&roots, &SimRecords::new()).await {
                Ok(n) => anyhow::bail!("carried {n} with one over the seam's value"),
                Err(e) => e.to_string(),
            };
            anyhow::ensure!(refused.contains("tasks.json"), "{refused}");
            Ok(())
        })
    }

    /// CP-T1.5 (goal 0d717247): the inventory's JSON files kept as Records are the ones [`FILES`] carries, name for
    /// name: a state file the inventory lists has its place here, and none is carried that it doesn't list.
    #[test]
    fn cp_t1_5_every_state_file_the_inventory_keeps_as_records_is_carried() -> anyhow::Result<()> {
        let inventory = include_str!("../../../../docs/records-inventory.md");
        let json = inventory.split("## JSON files kiln writes").nth(1).and_then(|s| s.split("\n## ").next()).ok_or_else(|| anyhow::anyhow!("the inventory's JSON files"))?;
        let listed: std::collections::BTreeSet<&str> = json
            .lines()
            .filter(|l| l.starts_with("| `"))
            .filter_map(|l| {
                let cells: Vec<&str> = l.split('|').map(str::trim).collect();
                match cells.as_slice() {
                    [_, name, _, _, kept, ..] if kept.contains("Records") => Some(name.trim_matches('`')),
                    _ => None,
                }
            })
            .collect();
        anyhow::ensure!(listed.len() > 20, "the inventory's state files read: {listed:?}");
        let carried: std::collections::BTreeSet<&str> = FILES.iter().flat_map(|f| f.names.iter().copied()).collect();
        let missing: Vec<&&str> = listed.difference(&carried).collect();
        let unlisted: Vec<&&str> = carried.difference(&listed).collect();
        anyhow::ensure!(missing.is_empty() && unlisted.is_empty(), "kept as Records in the inventory but not carried: {missing:?}; carried but not listed so: {unlisted:?}");
        Ok(())
    }
}
