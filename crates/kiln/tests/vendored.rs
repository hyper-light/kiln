//! kiln links one revision of each project it vendors and of each hyper-light product (CP-M1,
//! AC-M1.4; docs/vendoring.md). Every crate a vendored project provides is in Cargo.lock once, built
//! from its folder under vendor/, never also from crates.io or git; and every crate kiln takes from
//! a hyper-light repository by git comes from one commit of it, a repository kiln doesn't also
//! vendor. A second revision fails this test, and with it the gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};

/// Where Cargo.lock says a package without a `source` comes from: a path, here a vendored folder.
const FROM_ITS_FOLDER: &str = "its folder under vendor/";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// What vendor/MANIFEST.toml vendors.
struct Vendored {
    /// Each crate a vendored project provides, and that project: every package manifest under the
    /// project's folder, build output left out.
    crates: BTreeMap<String, String>,
    /// The hyper-light repositories vendored (`hyper-light/<name>`).
    repositories: BTreeSet<String>,
}

fn vendored(root: &Path) -> anyhow::Result<Vendored> {
    let text = std::fs::read_to_string(root.join("vendor/MANIFEST.toml")).context("vendor/MANIFEST.toml")?;
    let manifest = text.parse::<toml::Table>().context("vendor/MANIFEST.toml")?;
    let Some(projects) = manifest.get("projects").and_then(|p| p.as_table()) else {
        bail!("vendor/MANIFEST.toml has no [projects]")
    };
    let mut out = Vendored { crates: BTreeMap::new(), repositories: BTreeSet::new() };
    for (project, spec) in projects {
        let Some(path) = spec.get("path").and_then(|p| p.as_str()) else { bail!("[projects.{project}] has no path") };
        if let Some(repository) = spec.get("repository").and_then(|r| r.as_str()) {
            let source = format!("git+{repository}#");
            if let Some((repo, _)) = git_source(&source)
                && repo.starts_with("hyper-light/")
            {
                out.repositories.insert(repo);
            }
        }
        // The folder walked with a stack of its own, not by recursion.
        let mut todo = vec![root.join(path)];
        while let Some(dir) = todo.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    if entry.file_name() != "target" {
                        todo.push(path);
                    }
                    continue;
                }
                if entry.file_name() != "Cargo.toml" {
                    continue;
                }
                let package = std::fs::read_to_string(&path)?.parse::<toml::Table>().with_context(|| path.display().to_string())?;
                let Some(name) = package.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) else { continue };
                if let Some(other) = out.crates.insert(name.to_owned(), project.clone())
                    && &other != project
                {
                    bail!("two vendored projects, {other} and {project}, both provide the crate {name}");
                }
            }
        }
    }
    Ok(out)
}

/// The repository (`owner/name`, lowercase, no `.git`) and commit of a Cargo.lock git source,
/// whatever its scheme: `git+https://github.com/hyper-light/hyper-raft?rev=…#<commit>`,
/// `git+ssh://git@github.com/Hyper-Light/hyper-raft.git#<commit>`.
fn git_source(source: &str) -> Option<(String, &str)> {
    let rest = source.strip_prefix("git+")?;
    let (url, commit) = rest.rsplit_once('#')?;
    let url = url.split('?').next()?;
    let path = url.split_once("://").map_or(url, |(_, p)| p);
    let path = path.rsplit_once('@').map_or(path, |(_, p)| p);
    let (_host, repo) = path.split_once(['/', ':'])?;
    Some((repo.trim_matches('/').trim_end_matches(".git").to_ascii_lowercase(), commit))
}

/// What's wrong in a lock file:
/// - a crate a vendored project provides, or one a hyper-light repository provides by git, linked
///   more than once (another version, crates.io, a fork, a path), or a vendored one from anywhere
///   but its folder;
/// - a hyper-light repository linked at more than one commit, or both vendored and taken by git.
fn problems(lock: &str, v: &Vendored) -> anyhow::Result<Vec<String>> {
    let lock = lock.parse::<toml::Table>().context("Cargo.lock")?;
    let mut by_name: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut by_git: BTreeMap<String, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    for package in lock.get("package").and_then(|p| p.as_array()).into_iter().flatten() {
        let (Some(name), Some(version)) = (package.get("name").and_then(|v| v.as_str()), package.get("version").and_then(|v| v.as_str()))
        else {
            continue;
        };
        let source = package.get("source").and_then(|s| s.as_str());
        by_name.entry(name).or_default().push(format!("{version} from {}", source.unwrap_or(FROM_ITS_FOLDER)));
        if let Some((repo, commit)) = source.and_then(git_source)
            && repo.starts_with("hyper-light/")
        {
            by_git.entry(repo).or_default().entry(commit).or_default().push(name);
        }
    }
    // Who provides each crate checked: a vendored project, or a hyper-light repository by git.
    let mut checked: BTreeMap<&str, String> = v.crates.iter().map(|(n, p)| (n.as_str(), format!("vendored as {p}"))).collect();
    for (repo, commits) in &by_git {
        for name in commits.values().flatten() {
            checked.entry(name).or_insert_with(|| format!("from {repo}"));
        }
    }
    let mut out = Vec::new();
    for (name, whose) in &checked {
        let vendored = v.crates.contains_key(*name);
        match by_name.get(name).map(Vec::as_slice).unwrap_or_default() {
            [] => {}
            [one] if !vendored || one.ends_with(FROM_ITS_FOLDER) => {}
            [one] => out.push(format!("{name} ({whose}) is linked as {one}, not from vendor/")),
            many => out.push(format!("{name} ({whose}) is linked {} times: {}", many.len(), many.join("; "))),
        }
    }
    for (repo, commits) in &by_git {
        if commits.len() > 1 {
            let each: Vec<String> = commits.iter().map(|(c, crates)| format!("{c} ({})", crates.join(", "))).collect();
            out.push(format!("{repo} is linked at {} commits: {}", commits.len(), each.join("; ")));
        }
        if v.repositories.contains(repo) {
            out.push(format!("{repo} is vendored, and also linked from git"));
        }
    }
    Ok(out)
}

/// `path` resolved against `dir` without touching the disk: `..` and `.` taken out lexically.
fn resolve(dir: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for part in dir.join(path).components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Path dependencies a manifest (its text, the folder it's in) takes from outside the repository
/// at `root`: a copy of something there would be one no vendor/MANIFEST.toml pins, and Cargo.lock
/// doesn't say where a path package lives.
fn paths_outside(text: &str, dir: &Path, root: &Path) -> anyhow::Result<Vec<String>> {
    let manifest = text.parse::<toml::Table>()?;
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        tables.extend(manifest.get(key).and_then(|t| t.as_table()));
        tables.extend(manifest.get("workspace").and_then(|w| w.get(key)).and_then(|t| t.as_table()));
    }
    for (_, target) in manifest.get("target").and_then(|t| t.as_table()).into_iter().flatten() {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            tables.extend(target.get(key).and_then(|t| t.as_table()));
        }
    }
    tables.extend(manifest.get("patch").and_then(|p| p.as_table()).into_iter().flat_map(|p| p.values()).filter_map(|t| t.as_table()));
    let root = resolve(root, ".");
    let mut out = Vec::new();
    for table in tables {
        for (name, spec) in table {
            if let Some(path) = spec.get("path").and_then(|p| p.as_str())
                && !resolve(dir, path).starts_with(&root)
            {
                out.push(format!("{name} = {{ path = \"{path}\" }} leaves the repository"));
            }
        }
    }
    Ok(out)
}

fn kiln_lock() -> anyhow::Result<String> {
    std::fs::read_to_string(root().join("Cargo.lock")).context("Cargo.lock")
}

#[test]
fn ac_m1_4_kiln_links_one_revision_of_each_vendored_project_and_product() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    // The check sees what is vendored: Turso's four crates, and the hyper-light products Quiver
    // brings (CP-T2.1): Quiver's, Focal's bus's and hyper-raft's crates.
    for (name, project) in [
        ("turso_core", "turso"),
        ("turso_parser", "turso"),
        ("turso_macros", "turso"),
        ("turso_ext", "turso"),
        ("quiver", "quiver"),
        ("quiver-api", "quiver"),
        ("focal-bus", "focal-bus"),
        ("focal-memory", "focal-bus"),
        ("hyper-rt", "hyper-raft"),
        ("hyper-transport", "hyper-raft"),
        ("hyper-durable", "hyper-raft"),
        // vorpal's crates, the tree-sitter runtime it carries and its grammars (W26-R07).
        ("vorpal-core", "vorpal"),
        ("vorpal-kg", "vorpal"),
        ("tree-sitter", "vorpal"),
        ("tree-sitter-rust", "vorpal"),
    ] {
        ensure!(v.crates.get(name).map(String::as_str) == Some(project), "{name} isn't among {project}'s vendored crates: {:?}", v.crates);
    }
    ensure!(["hyper-light/quiver", "hyper-light/focal", "hyper-light/hyper-raft", "hyper-light/vorpal"].iter().all(|r| v.repositories.contains(*r)), "{:?}", v.repositories);
    let lock = kiln_lock()?;
    // kiln links them from their folders: none from git.
    ensure!(!lock.lines().any(|l| l.starts_with("source = \"git+https://github.com/hyper-light/")), "a hyper-light product linked from git");
    let found = problems(&lock, &v)?;
    ensure!(found.is_empty(), "kiln links more than one revision:\n{}", found.join("\n"));
    Ok(())
}

#[test]
fn ac_m1_4_no_manifest_of_kiln_takes_a_path_dependency_from_outside_the_repository() -> anyhow::Result<()> {
    let root = root();
    let mut manifests = vec![root.join("Cargo.toml")];
    for entry in std::fs::read_dir(root.join("crates"))? {
        let manifest = entry?.path().join("Cargo.toml");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    ensure!(manifests.len() >= 4, "kiln's manifests weren't found: {manifests:?}");
    let mut found = Vec::new();
    for manifest in &manifests {
        let dir = manifest.parent().context("a manifest's folder")?;
        found.extend(paths_outside(&std::fs::read_to_string(manifest)?, dir, &root)?.into_iter().map(|p| format!("{}: {p}", manifest.display())));
    }
    ensure!(found.is_empty(), "{}", found.join("\n"));
    // And the check sees one: a product's crate by path from a clone beside the repository.
    let planted = "[dependencies]\namphora = { path = \"../../../amphora/crates/amphora\" }\n";
    let found = paths_outside(planted, &root.join("crates/kiln"), &root)?;
    ensure!(found.iter().any(|p| p.starts_with("amphora = { path = \"../../../amphora")), "{found:?}");
    Ok(())
}

/// crates.io has unrelated crates under some vendored names (`quiver` 0.1 to 0.6, `hyper-rt`):
/// one resolved there in the vendored crate's place, as a newer compatible version would be,
/// fails the check.
#[test]
fn ac_m1_4_an_unrelated_crates_io_crate_under_a_vendored_name_fails_the_check() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let mut lock = kiln_lock()?;
    let ours = "[[package]]\nname = \"quiver\"\nversion = \"0.1.0\"\n";
    ensure!(lock.contains(ours), "quiver isn't in the lock as kiln's");
    lock = lock.replacen(ours, "[[package]]\nname = \"quiver\"\nversion = \"0.1.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n", 1);
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p.starts_with("quiver (vendored as quiver) is linked as 0.1.1 from registry+")), "{found:?}");
    Ok(())
}

#[test]
fn ac_m1_4_a_second_revision_of_a_vendored_crate_fails_the_check() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()? + "\n[[package]]\nname = \"turso_core\"\nversion = \"0.8.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    let found = problems(&lock, &v)?;
    ensure!(
        found.iter().any(|p| p.starts_with("turso_core (vendored as turso) is linked 2 times") && p.contains("0.8.0 from registry+")),
        "{found:?}"
    );
    Ok(())
}

#[test]
fn ac_m1_4_a_vendored_crate_taken_from_git_fails_the_check() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()?.replacen(
        "name = \"turso_parser\"\nversion = \"0.9.0-pre.1\"\n",
        "name = \"turso_parser\"\nversion = \"0.9.0-pre.1\"\nsource = \"git+https://github.com/tursodatabase/turso?rev=3e09a82#3e09a82\"\n",
        1,
    );
    ensure!(lock.contains("source = \"git+https://github.com/tursodatabase/turso"), "the planted source isn't in the lock");
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p.starts_with("turso_parser (vendored as turso) is linked as 0.9.0-pre.1 from git+")), "{found:?}");
    Ok(())
}

/// A crate of a hyper-light product kiln takes by git (none, since hyper-raft is vendored: a stand-in,
/// `hyper-light/shards`'s), as Cargo.lock would say it.
fn by_git(name: &str, url: &str, commit: &str) -> String {
    format!("\n[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\nsource = \"git+{url}#{commit}\"\n")
}

#[test]
fn ac_m1_4_a_product_both_vendored_and_taken_by_git_fails_the_check() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()? + &by_git("hyper-sim", "https://github.com/hyper-light/hyper-raft?rev=0123456", "0123456789abcdef0123456789abcdef01234567");
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p == "hyper-light/hyper-raft is vendored, and also linked from git"), "{found:?}");
    Ok(())
}

#[test]
fn ac_m1_4_a_product_by_ssh_in_another_letter_case_is_the_same_product() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()?
        + &by_git("shards-api", "https://github.com/hyper-light/shards?rev=0123456", "0123456789abcdef0123456789abcdef01234567")
        + &by_git("shards-sim", "ssh://git@github.com/Hyper-Light/shards.git", "fedcba9876543210fedcba9876543210fedcba98");
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p.starts_with("hyper-light/shards is linked at 2 commits") && p.contains("fedcba9876543210fedcba9876543210fedcba98 (shards-sim)")), "{found:?}");
    Ok(())
}

#[test]
fn ac_m1_4_a_product_crate_from_a_fork_is_a_second_copy() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()?
        + &by_git("shards-api", "https://github.com/hyper-light/shards?rev=0123456", "0123456789abcdef0123456789abcdef01234567")
        + "\n[[package]]\nname = \"shards-api\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/someone/shards?rev=0123456#0123456\"\n";
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p.starts_with("shards-api (from hyper-light/shards) is linked 2 times") && p.contains("github.com/someone/shards")), "{found:?}");
    // And a vendored product's crate from a fork beside its folder's is a second copy too.
    let lock = kiln_lock()? + "\n[[package]]\nname = \"hyper-rt\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/someone/hyper-raft?rev=0123456#0123456\"\n";
    let found = problems(&lock, &v)?;
    ensure!(found.iter().any(|p| p.starts_with("hyper-rt (vendored as hyper-raft) is linked 2 times") && p.contains("github.com/someone/hyper-raft")), "{found:?}");
    Ok(())
}

#[test]
fn ac_m1_4_a_product_linked_at_two_commits_fails_the_check() -> anyhow::Result<()> {
    let v = vendored(&root())?;
    let lock = kiln_lock()?
        + &by_git("shards-api", "https://github.com/hyper-light/shards?rev=fedcba9", "fedcba9876543210fedcba9876543210fedcba98")
        + &by_git("shards-sim", "https://github.com/hyper-light/shards?rev=0123456", "0123456789abcdef0123456789abcdef01234567");
    let found = problems(&lock, &v)?;
    ensure!(
        found.iter().any(|p| p.starts_with("hyper-light/shards is linked at 2 commits") && p.contains("0123456789abcdef0123456789abcdef01234567 (shards-sim)")),
        "{found:?}"
    );
    Ok(())
}
