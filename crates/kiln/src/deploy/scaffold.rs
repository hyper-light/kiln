//! An app's source repository, made with it (docs/deploy-api.md §3.1, step 4): one of kiln's own
//! repositories, or one on GitHub, a GitLab or Bitbucket (made there through P3's clients, then
//! one of kiln's repositories as any upstream is), named as its template says (`acme/{app}`). Its
//! first commit holds the files of the template's starter repository as they are (its branch's
//! tree, not its history), with `{{kiln.app}}`, `{{kiln.project}}`, `{{kiln.owner}}` and
//! `{{kiln.repository}}` filled in, in files and in file names. The starter's `.kiln/ci.yaml`
//! builds what it holds, so the app's first build starts with its first commit.
//!
//! The starter is data: kiln copies its files and runs nothing from it. Links stay links, as git
//! keeps them; files keep their modes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Deploy;
use super::flow::Fail;
use super::model::{App, KILN_HOST, RepoRef, RepoTemplate};
use http::StatusCode;

/// The most a starter may hold.
const MAX_FILES: usize = 20_000;
const MAX_BYTES: u64 = 256 << 20;

/// The repositories being made now, by name: one maker each.
static MAKING: parking_lot::Mutex<BTreeSet<String>> = parking_lot::Mutex::new(BTreeSet::new());

/// A repository's name held while it is made, let go however that ends.
struct Making(String);

impl Making {
    fn hold(name: &str) -> Option<Making> {
        MAKING.lock().insert(name.to_ascii_lowercase()).then(|| Making(name.to_ascii_lowercase()))
    }
}

impl Drop for Making {
    fn drop(&mut self) {
        MAKING.lock().remove(&self.0);
    }
}

/// The repository a template names for an app.
pub fn name_for(t: &RepoTemplate, app: &App) -> String {
    t.name.replace("{app}", &app.name).replace("{project}", &app.project)
}

/// What `{{kiln.*}}` stands for in an app's starter.
fn values(app: &App, repository: &str) -> [(&'static str, String); 4] {
    [
        ("{{kiln.app}}", app.name.clone()),
        ("{{kiln.project}}", app.project.clone()),
        ("{{kiln.owner}}", app.owners.first().cloned().unwrap_or_default()),
        ("{{kiln.repository}}", repository.to_owned()),
    ]
}

fn fill(text: &str, values: &[(&'static str, String)]) -> String {
    values.iter().fold(text.to_owned(), |t, (k, v)| t.replace(k, v))
}

/// Fills in a starter's files where they are, under `root` (but `.git`): their text, then their
/// names, deepest first. Files that aren't UTF-8 are left as they are. How many files it holds.
fn fill_tree(root: &Path, values: &[(&'static str, String)]) -> anyhow::Result<usize> {
    let (mut files, mut bytes, mut renames) = (0usize, 0u64, Vec::<PathBuf>::new());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let (path, kind) = (entry.path(), entry.file_type()?);
            if entry.file_name().to_string_lossy().contains("{{kiln.") {
                renames.push(path.clone());
            }
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
                continue;
            }
            files += 1;
            bytes += entry.metadata()?.len();
            anyhow::ensure!(files <= MAX_FILES && bytes <= MAX_BYTES, "the starter is bigger than kiln copies ({MAX_FILES} files, {} MB)", MAX_BYTES >> 20);
            // A link is kept as git keeps it: where it points, never what's there.
            if !kind.is_file() {
                continue;
            }
            let text = match String::from_utf8(std::fs::read(&path)?) {
                Ok(text) if text.contains("{{kiln.") => text,
                _ => continue,
            };
            std::fs::write(&path, fill(&text, values))?;
        }
    }
    // The deepest first, so a directory's own name changes after what it holds.
    renames.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for from in renames {
        let name = from.file_name().map(|n| fill(&n.to_string_lossy(), values)).unwrap_or_default();
        anyhow::ensure!(!name.is_empty() && name != "." && name != ".." && !name.contains('/'), "{}: its name filled in ({name}) isn't a file name", from.display());
        let to = from.with_file_name(&name);
        anyhow::ensure!(!to.exists(), "{}: {name} is there already", from.display());
        std::fs::rename(&from, &to)?;
    }
    Ok(files)
}

/// Whether `s` may name a branch or a tag here: git's rules, kept simple.
pub fn ref_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '/', '.'])
        && !s.ends_with(['/', '.'])
        && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with(".lock")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// What a template says of the repositories it makes, checked when it is put: a name that is one
/// for every app, a starter kiln may copy, branches git takes.
pub fn check(d: &Deploy, t: &RepoTemplate) -> Result<(), String> {
    let example = t.name.replace("{app}", "app").replace("{project}", "project");
    if t.host == KILN_HOST {
        if crate::ci::hosting::path_of(&d.hub, &example).is_none() {
            return Err(format!("repository.name {}: `owner/name`, of letters, digits, `-`, `_` and `.`, with {{app}} and {{project}} filled in", t.name));
        }
    } else if super::model::remote_host(&t.host) {
        super::remote::name_ok(&t.host, &example).map_err(|e| format!("repository.name {} (with {{app}} and {{project}} filled in): {e}", t.name))?;
    } else {
        return Err(format!("repository.host {}: kiln (its own), github, gitlab or bitbucket", t.host));
    }
    if let Some(from) = &t.starter {
        crate::ci::hosting::importable(from).map_err(|e| format!("repository.starter: {e}"))?;
    }
    for (what, r) in [("branch", Some(&t.branch)), ("starter_ref", t.starter_ref.as_ref())] {
        if let Some(r) = r.filter(|r| !ref_name(r)) {
            return Err(format!("repository.{what} {r}: a branch or a tag (letters, digits, `-`, `_`, `.` and `/`)"));
        }
    }
    Ok(())
}

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::BAD_GATEWAY, format!("{e:#}"))
}

/// Makes the app's repository from its template: the native repository and its row, its first
/// commit the starter's files filled in (empty, when the template names no starter). Its name.
pub async fn create(d: &Deploy, t: &RepoTemplate, app: &App, by: &str) -> Result<String, Fail> {
    if t.host != KILN_HOST {
        return create_there(d, t, app, by).await;
    }
    let name = name_for(t, app);
    let path = crate::ci::hosting::path_of(&d.hub, &name)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("{name}: a repository is `owner/name`, of letters, digits, `-`, `_` and `.`")))?;
    let taken = || (StatusCode::CONFLICT, format!("{name} exists already: give it as the app's source, or have the template name another"));
    let _making = Making::hold(&name).ok_or_else(taken)?;
    let repos = d.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if repos.iter().any(|r| r.name.eq_ignore_ascii_case(&name)) || path.exists() {
        return Err(taken());
    }
    let seeded = match &t.starter {
        Some(from) => Some(seed(d, t, from, app, &name, by).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the starter {from}: {e:#}")))?),
        None => None,
    };
    let made = async {
        crate::ci::hosting::create(&d.hub, &name, &t.branch, None).await?;
        if let Some(dir) = &seeded {
            let (dir_s, path_s) = (dir.to_string_lossy().into_owned(), path.to_string_lossy().into_owned());
            let refspec = format!("HEAD:refs/heads/{}", t.branch);
            crate::gitops::run_git(&d.hub, &["-C", &dir_s, "push", "--quiet", "--", &path_s, &refspec], Duration::from_secs(600)).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Some(dir) = &seeded {
        let _ = std::fs::remove_dir_all(dir);
    }
    if let Err(e) = made {
        // Nothing half made stays: the repository is new, and no one has it yet.
        let _ = std::fs::remove_dir_all(&path);
        return Err(fail(e));
    }
    let repo = d
        .hub
        .store
        .upsert_repo(crate::store::Repo {
            id: crate::util::new_id(),
            name: name.clone(),
            clone_url: crate::ci::hosting::clone_url(&path),
            branches: Vec::new(),
            poll_seconds: d.hub.cfg.gitops.poll_seconds,
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
            mode: "native".into(),
            default_branch: Some(t.branch.clone()),
            mirror: None,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    d.hub.store.audit(by.to_owned(), "repo.add".into(), format!("{name}, the source of app {}{}", app.name, t.starter.as_ref().map(|s| format!(", from {s}")).unwrap_or_default())).await;
    if seeded.is_some() {
        // In a task of its own: what a push starts goes deep, and the app's answer needn't wait.
        let (hub, branch) = (d.hub.clone(), t.branch.clone());
        crate::rt::spawn(async move {
            let (p, head) = (path.to_string_lossy().into_owned(), format!("refs/heads/{branch}"));
            match crate::gitops::run_git(&hub, &["-C", &p, "rev-parse", "--verify", &head], crate::gitops::LS_REMOTE_TIMEOUT).await {
                Ok(out) => first_build(&hub, &repo, &branch, String::from_utf8_lossy(&out).trim().to_owned()).await,
                Err(e) => tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "an app's new repository: its first commit isn't there"),
            }
        });
    }
    d.hub.gitops_kick.notify();
    Ok(name)
}

/// Makes the app's repository on another host (GitHub, a GitLab, Bitbucket): there, private, its
/// first commit the starter's files filled in (pushed with credentials that write that
/// repository), then one of kiln's repositories as any upstream is, so CI runs from it. A name
/// the host has already is refused before anything is made; one made whose first commit didn't
/// reach it is deleted again. Its name.
async fn create_there(d: &Deploy, t: &RepoTemplate, app: &App, by: &str) -> Result<String, Fail> {
    let name = name_for(t, app);
    super::remote::name_ok(&t.host, &name).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let on = super::remote::host_label(&t.host);
    let taken = || (StatusCode::CONFLICT, format!("{name} exists already on {on}: give it as the app's source, or have the template name another"));
    let _making = Making::hold(&name).ok_or_else(taken)?;
    let repos = d.hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if repos.iter().any(|r| r.name.eq_ignore_ascii_case(&name)) {
        return Err((StatusCode::CONFLICT, format!("kiln has a repository {name} already: give it as the app's source, or have the template name another")));
    }
    let rr = RepoRef { host: t.host.clone(), repo: name.clone(), branch: t.branch.clone(), root: String::new() };
    let there = super::remote::open(d, &rr).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if there.exists().await.map_err(fail)? {
        return Err(taken());
    }
    let seeded = match &t.starter {
        Some(from) => Some(seed(d, t, from, app, &name, by).await.map_err(|e| (StatusCode::BAD_GATEWAY, format!("the starter {from}: {e:#}")))?),
        None => None,
    };
    let made = async {
        there.make(&format!("{}: {}'s source, made by kiln", app.name, app.project)).await?;
        let Some(dir) = &seeded else { return anyhow::Ok(None) };
        let (dir_s, head) = (dir.to_string_lossy().into_owned(), format!("HEAD:refs/heads/{}", t.branch));
        let pushed = there.push_from(dir, &head).await;
        let sha = crate::gitops::run_git(&d.hub, &["-C", &dir_s, "rev-parse", "HEAD"], Duration::from_secs(30)).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned());
        if let Err(e) = pushed {
            // Nothing half made stays: the repository is new, and empty.
            let gone = there.delete().await;
            anyhow::bail!("{name} was made on {on}, but its first commit didn't reach it ({e:#}){}", if gone.is_ok() { "; it's deleted again" } else { ": delete it there, or give it as the app's source" });
        }
        anyhow::Ok(sha.ok())
    }
    .await;
    if let Some(dir) = &seeded {
        let _ = std::fs::remove_dir_all(dir);
    }
    let first = made.map_err(fail)?;
    let repo = d
        .hub
        .store
        .upsert_repo(crate::store::Repo {
            id: crate::util::new_id(),
            name: name.clone(),
            clone_url: there.url(),
            branches: Vec::new(),
            poll_seconds: d.hub.cfg.gitops.poll_seconds,
            enabled: true,
            created_at: crate::util::now_unix(),
            last_polled_at: None,
            last_error: None,
            mode: "upstream".into(),
            default_branch: Some(t.branch.clone()),
            mirror: None,
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    d.hub.store.audit(by.to_owned(), "repo.add".into(), format!("{name} on {on}, the source of app {}{}", app.name, t.starter.as_ref().map(|s| format!(", from {s}")).unwrap_or_default())).await;
    if let Some(sha) = first {
        let (hub, branch) = (d.hub.clone(), t.branch.clone());
        crate::rt::spawn(async move { first_build(&hub, &repo, &branch, sha).await });
    }
    d.hub.gitops_kick.notify();
    Ok(name)
}

/// Starts what a push of the repository's first commit (`sha`) starts (its `.kiln/` pipelines,
/// its workflows): the poller takes a new repository's refs as they are, and builds from the next
/// change on.
async fn first_build(hub: &crate::hub::Hub, repo: &crate::store::Repo, branch: &str, sha: String) {
    let head = format!("refs/heads/{branch}");
    let started = async {
        // Whoever records the ref's move first triggers it; the poller won't again.
        if !hub.store.advance_ref(repo.id.clone(), head.clone(), sha.clone()).await? {
            return anyhow::Ok(0);
        }
        let push = crate::gitops::Trigger { event: "push", branch: branch.to_owned(), sha, before: None, pipeline: None, payload: None };
        anyhow::Ok(crate::gitops::trigger(hub, repo, push).await?.len())
    }
    .await;
    match started {
        Ok(runs) => tracing::info!(repo = %repo.name, runs, "an app's new repository: its first commit builds"),
        Err(e) => tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "an app's new repository: its first commit started no build"),
    }
}

/// The starter's files as one new commit, in a scratch repository (removed by the caller): cloned,
/// filled in, its history left behind.
async fn seed(d: &Deploy, t: &RepoTemplate, from: &str, app: &App, name: &str, by: &str) -> anyhow::Result<PathBuf> {
    crate::ci::hosting::importable(from).map_err(anyhow::Error::msg)?;
    let dir = d.hub.cfg.data("tmp").join(format!("starter-{}", crate::util::new_id()));
    let seeded = async {
        std::fs::create_dir_all(d.hub.cfg.data("tmp"))?;
        let dir_s = dir.to_string_lossy().into_owned();
        let mut clone = vec!["clone", "--quiet", "--depth=1", "--no-tags", "--single-branch"];
        if let Some(r) = &t.starter_ref {
            clone.extend(["--branch", r.as_str()]);
        }
        clone.extend(["--", from, &dir_s]);
        crate::gitops::run_git(&d.hub, &clone, Duration::from_secs(600)).await?;
        std::fs::remove_dir_all(dir.join(".git"))?;
        let (root, vals) = (dir.clone(), values(app, name));
        let files = crate::rt::blocking(move || fill_tree(&root, &vals)).await??;
        anyhow::ensure!(files > 0, "it holds no files");
        let person = super::gates::person(by);
        let email = format!("{}@kiln", person.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect::<String>());
        let (user, mail) = (format!("user.name={person}"), format!("user.email={email}"));
        let message = format!("Start {} from {from}\n\nKiln-Requested-By: {by}\n", app.name);
        let git = |args: Vec<&str>| {
            let mut all = vec!["-C", dir_s.as_str(), "-c", user.as_str(), "-c", mail.as_str(), "-c", "commit.gpgsign=false"];
            all.extend(args);
            all.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        for args in [vec!["init", "--quiet", "--initial-branch", t.branch.as_str()], vec!["add", "--all"], vec!["commit", "--quiet", "--no-verify", "-m", message.as_str()]] {
            let args = git(args);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            crate::gitops::run_git(&d.hub, &refs, Duration::from_secs(120)).await?;
        }
        anyhow::Ok(())
    }
    .await;
    match seeded {
        Ok(()) => Ok(dir),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::deploy::model::Package;

    fn app() -> App {
        App {
            name: "web".into(),
            project: "shop".into(),
            owners: vec!["payments".into()],
            package: Package::Git { repo: "x".into(), path: String::new(), set: BTreeMap::new() },
            namespace: None,
            properties: BTreeMap::new(),
            template: Some("service".into()),
            contacts: Default::default(),
            source: None,
            archived: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_starter_is_filled_in_for_its_app_in_its_files_and_their_names() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir()?;
        let root = tmp.path();
        std::fs::create_dir_all(root.join("charts/{{kiln.app}}/templates"))?;
        std::fs::write(root.join("charts/{{kiln.app}}/Chart.yaml"), "name: {{kiln.app}}\ndescription: {{kiln.project}}, by {{kiln.owner}} ({{kiln.repository}})\n")?;
        std::fs::write(root.join("charts/{{kiln.app}}/templates/svc.yaml"), "name: {{ .Release.Name }}\n")?;
        std::fs::write(root.join("logo.bin"), [0xff_u8, 0xfe, b'{', b'{', b'k'])?;
        std::fs::write(root.join("build.sh"), "echo {{kiln.app}}\n")?;
        std::fs::set_permissions(root.join("build.sh"), std::fs::Permissions::from_mode(0o755))?;
        std::os::unix::fs::symlink("/etc/hostname", root.join("host-{{kiln.app}}"))?;
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/config"), "{{kiln.app}}\n")?;

        let n = fill_tree(root, &values(&app(), "shop/web"))?;
        assert_eq!(n, 5, "every file but git's own");
        assert_eq!(std::fs::read_to_string(root.join("charts/web/Chart.yaml"))?, "name: web\ndescription: shop, by payments (shop/web)\n");
        assert_eq!(std::fs::read_to_string(root.join("charts/web/templates/svc.yaml"))?, "name: {{ .Release.Name }}\n", "Helm's own left as it is");
        assert_eq!(std::fs::read(root.join("logo.bin"))?, [0xff_u8, 0xfe, b'{', b'{', b'k'], "not text: as it is");
        assert_eq!(std::fs::read_to_string(root.join("build.sh"))?, "echo web\n");
        assert_eq!(std::fs::metadata(root.join("build.sh"))?.permissions().mode() & 0o777, 0o755, "its mode kept");
        assert_eq!(std::fs::read_link(root.join("host-web"))?, Path::new("/etc/hostname"), "a link renamed, still a link");
        assert_eq!(std::fs::read_to_string(root.join(".git/config"))?, "{{kiln.app}}\n", "git's own untouched");
        assert!(!root.join("charts/{{kiln.app}}").exists());
        Ok(())
    }

    #[test]
    fn a_repository_is_made_by_one_maker_at_a_time() {
        let first = Making::hold("making-test/Web");
        assert!(first.is_some());
        assert!(Making::hold("making-test/web").is_none(), "the same name, however it is written");
        drop(first);
        assert!(Making::hold("making-test/web").is_some(), "free again once the first is done");
    }

    #[test]
    fn a_template_names_each_app_s_repository() {
        let t = RepoTemplate { name: "{project}/{app}".into(), starter: None, starter_ref: None, branch: "main".into(), host: KILN_HOST.into() };
        assert_eq!(name_for(&t, &app()), "shop/web");
        for good in ["main", "release/v1", "v1.2.3", "feature_x"] {
            assert!(ref_name(good), "{good}");
        }
        for wrong in ["", "-x", "a..b", "a b", "x.lock", "/x", "x/", "a//b", ".hidden"] {
            assert!(!ref_name(wrong), "{wrong}");
        }
    }
}
