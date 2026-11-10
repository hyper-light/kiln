//! Where a package comes from, read into files for [`super::detect::read`]: a link (a GitHub page
//! or any git URL, fetched at one commit with the server's git credentials), an upload (a `.zip`, a
//! `.tar.gz` or one `.md`), words written in the page, or a folder on the agent's machine.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::detect::{File, MAX_FILE, MAX_TOTAL};
use crate::hub::Hub;

/// Where a package came from, as kept with it and shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Source {
    /// "link", "folder", "upload" or "write".
    pub kind: String,
    /// What it's shown as: `cursor/plugins`, `adacachy:~/skills/perf`, `release-notes.md`.
    pub label: String,
    /// A link's git URL, to fetch again for its updates.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The branch or tag the link named ("": its default branch).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_ref: String,
    /// The folder or file in it the link named.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// A folder's machine.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine: String,
}

/// A link, read: its git URL, the branch or tag it names, the folder or file in it, and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    pub git_ref: String,
    pub path: String,
    pub label: String,
}

/// Reads a link: `github.com/owner/repo`, with `/tree/<ref>/<path>` or `/blob/<ref>/<path>`, with or
/// without its scheme; or any other git URL (https, ssh, `git@host:path`) as it is.
pub fn link(text: &str) -> Result<Link, String> {
    let t = text.trim().trim_end_matches('/');
    if t.is_empty() {
        return Err("a link to a repository".into());
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a link has no spaces".into());
    }
    let bare = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")).unwrap_or(t);
    if let Some(rest) = bare.strip_prefix("github.com/").or_else(|| bare.strip_prefix("www.github.com/")) {
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let (Some(owner), Some(repo)) = (segs.first(), segs.get(1)) else { return Err("a GitHub link names its owner and repository: github.com/owner/repo".into()) };
        let repo = repo.trim_end_matches(".git");
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && !s.starts_with('.');
        if !ok(owner) || !ok(repo) {
            return Err(format!("{owner}/{repo} isn't a GitHub repository's name"));
        }
        let (git_ref, path) = match segs.get(2) {
            Some(&("tree" | "blob")) => (segs.get(3).map(|s| (*s).to_owned()).unwrap_or_default(), segs.get(4..).map(|p| p.join("/")).unwrap_or_default()),
            Some(_) => return Err("a GitHub link to a folder is …/tree/<branch>/<folder>".into()),
            None => (String::new(), String::new()),
        };
        if path.split('/').any(|s| s == "..") {
            return Err("a link's folder stays in its repository".into());
        }
        return Ok(Link { url: format!("https://github.com/{owner}/{repo}.git"), git_ref, label: format!("{owner}/{repo}"), path });
    }
    if t.starts_with("https://") || t.starts_with("http://") || t.starts_with("ssh://") || t.starts_with("git@") {
        let label = t.trim_end_matches(".git").rsplit(['/', ':']).take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
        return Ok(Link { url: t.to_owned(), git_ref: String::new(), path: String::new(), label });
    }
    Err("a link starts https://, ssh:// or git@ (or github.com/owner/repo)".into())
}

/// Every file under `dir`, but `.git`'s, by its path from `dir`: what's past [`MAX_FILE`] is left
/// out (a package never holds it: one that needs it says so when read), and reading stops past
/// four packages' worth.
pub fn walk(dir: &Path) -> std::io::Result<Vec<File>> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&d)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let name = e.file_name();
            if name == ".git" {
                continue;
            }
            let t = e.file_type()?;
            let p = e.path();
            if t.is_dir() {
                stack.push(p);
                continue;
            }
            if !t.is_file() {
                continue;
            }
            let meta = e.metadata()?;
            if meta.len() > MAX_FILE {
                continue;
            }
            total = total.saturating_add(meta.len());
            if total > MAX_TOTAL.saturating_mul(4) {
                return Err(std::io::Error::other(format!("more than {} bytes", MAX_TOTAL.saturating_mul(4))));
            }
            let Ok(rel) = p.strip_prefix(dir) else { continue };
            let path = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            out.push(File { path, bytes: std::fs::read(&p)?, exec: executable(&meta) });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(unix)]
fn executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

/// A link fetched at one commit: its files and the commit. Into a folder of its own under the
/// server's data (never `/tmp`), gone when this returns.
pub async fn fetch(hub: &Hub, l: &Link) -> anyhow::Result<(Vec<File>, String)> {
    let dir = hub.cfg.data("skills").join(format!("fetch-{}", crate::util::new_id()));
    let d = dir.to_string_lossy().into_owned();
    let got = async {
        crate::rt::blocking({
            let dir = dir.clone();
            move || std::fs::create_dir_all(&dir)
        })
        .await??;
        let timeout = crate::gitops::GIT_TIMEOUT;
        let branch = if l.git_ref.is_empty() { None } else { Some(l.git_ref.as_str()) };
        let mut clone = vec!["clone", "--quiet", "--depth", "1", "--no-tags"];
        if let Some(b) = branch {
            clone.extend(["--branch", b]);
        }
        clone.extend(["--", l.url.as_str(), d.as_str()]);
        if let Err(e) = crate::gitops::run_git(hub, &clone, timeout).await {
            // A commit, not a branch or tag: fetched by itself.
            let Some(sha) = branch.filter(|b| b.len() >= 7 && b.chars().all(|c| c.is_ascii_hexdigit())) else { return Err(e) };
            crate::gitops::run_git(hub, &["init", "--quiet", &d], timeout).await?;
            crate::gitops::run_git(hub, &["-C", &d, "fetch", "--quiet", "--depth", "1", "--", &l.url, sha], timeout).await?;
            crate::gitops::run_git(hub, &["-C", &d, "checkout", "--quiet", "--detach", "FETCH_HEAD"], timeout).await?;
        }
        let sha = String::from_utf8_lossy(&crate::gitops::run_git(hub, &["-C", &d, "rev-parse", "HEAD"], timeout).await?).trim().to_owned();
        let files = crate::rt::blocking({
            let dir = dir.clone();
            move || walk(&dir)
        })
        .await??;
        anyhow::Ok((files, sha))
    }
    .await;
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(&dir)).await;
    got
}

/// The commit a link's branch is at now, for an update: `None` when its URL doesn't say.
pub async fn head(hub: &Hub, url: &str, git_ref: &str) -> anyhow::Result<Option<String>> {
    let refs = crate::gitops::ls_remote(hub, url).await?;
    let want = if git_ref.is_empty() { None } else { Some(git_ref) };
    Ok(match want {
        None => refs.get("HEAD").cloned(),
        Some(r) => refs.get(&format!("refs/heads/{r}")).or_else(|| refs.get(&format!("refs/tags/{r}"))).cloned(),
    })
}

/// An upload's files: a `.zip`, a `.tar.gz` (`.tgz`), or one `.md`, by its name. An archive whose
/// every file is in one folder is read from inside it.
pub fn unpack(name: &str, bytes: &[u8]) -> Result<Vec<File>, String> {
    let lower = name.to_ascii_lowercase();
    let mut files = if lower.ends_with(".md") {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        vec![File { path: base.to_owned(), bytes: bytes.to_vec(), exec: false }]
    } else if lower.ends_with(".zip") {
        unzip(bytes)?
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        untar(bytes)?
    } else {
        return Err("upload a .zip, a .tar.gz or a .md".into());
    };
    files.retain(|f| !f.path.split('/').any(|s| s == ".git" || s == "__MACOSX") && !f.path.ends_with(".DS_Store"));
    let first = |f: &File| f.path.split('/').next().unwrap_or("").to_owned();
    if let Some(top) = files.first().map(first)
        && files.len() > 1
        && files.iter().all(|f| f.path.contains('/') && first(f) == top)
    {
        for f in &mut files {
            f.path = f.path.get(top.len() + 1..).unwrap_or_default().to_owned();
        }
    }
    if files.is_empty() {
        return Err("the upload holds no files".into());
    }
    Ok(files)
}

fn safe(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    (!parts.is_empty() && !parts.contains(&"..") && !p.starts_with('/')).then(|| parts.join("/"))
}

fn unzip(bytes: &[u8]) -> Result<Vec<File>, String> {
    use std::io::Read as _;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("not a zip: {e}"))?;
    let mut out = Vec::new();
    let mut total = 0u64;
    for i in 0..z.len() {
        let mut e = z.by_index(i).map_err(|e| format!("the zip: {e}"))?;
        if e.is_dir() || e.size() > MAX_FILE {
            continue;
        }
        let Some(path) = safe(e.name()) else { continue };
        total = total.saturating_add(e.size());
        if total > MAX_TOTAL {
            return Err(format!("it unpacks to more than {MAX_TOTAL} bytes"));
        }
        let exec = e.unix_mode().is_some_and(|m| m & 0o111 != 0);
        let mut b = Vec::new();
        e.by_ref().take(MAX_FILE).read_to_end(&mut b).map_err(|e| format!("the zip: {e}"))?;
        out.push(File { path, bytes: b, exec });
    }
    Ok(out)
}

fn untar(bytes: &[u8]) -> Result<Vec<File>, String> {
    use std::io::Read as _;
    let mut a = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut out = Vec::new();
    let mut total = 0u64;
    for e in a.entries().map_err(|e| format!("not a .tar.gz: {e}"))? {
        let mut e = e.map_err(|e| format!("the archive: {e}"))?;
        if !e.header().entry_type().is_file() || e.size() > MAX_FILE {
            continue;
        }
        let Some(path) = e.path().ok().and_then(|p| safe(&p.to_string_lossy())) else { continue };
        total = total.saturating_add(e.size());
        if total > MAX_TOTAL {
            return Err(format!("it unpacks to more than {MAX_TOTAL} bytes"));
        }
        let exec = e.header().mode().is_ok_and(|m| m & 0o111 != 0);
        let mut b = Vec::new();
        e.by_ref().take(MAX_FILE).read_to_end(&mut b).map_err(|e| format!("the archive: {e}"))?;
        out.push(File { path, bytes: b, exec });
    }
    Ok(out)
}

/// A skill written in the page: its `SKILL.md`, frontmatter and body.
pub fn written(name: &str, description: &str, body: &str) -> Result<Vec<File>, String> {
    let n = super::detect::slug(name);
    if n.is_empty() {
        return Err("name it: letters, digits and dashes".into());
    }
    let d = description.trim().replace('\n', " ");
    if d.is_empty() {
        return Err("say when to use it: its description is how an agent finds it".into());
    }
    let quoted = serde_json::to_string(&d).unwrap_or_default();
    Ok(vec![File { path: format!("{n}.md"), bytes: format!("---\nname: {n}\ndescription: {quoted}\n---\n\n{}\n", body.trim()).into_bytes(), exec: false }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_a_repository_a_branch_and_a_folder() -> anyhow::Result<()> {
        let l = link("https://github.com/cursor/plugins/tree/main/pstack").map_err(anyhow::Error::msg)?;
        assert_eq!((l.url.as_str(), l.git_ref.as_str(), l.path.as_str(), l.label.as_str()), ("https://github.com/cursor/plugins.git", "main", "pstack", "cursor/plugins"));
        let l = link("github.com/t3dotgg/tstack/").map_err(anyhow::Error::msg)?;
        assert_eq!((l.url.as_str(), l.git_ref.as_str(), l.path.as_str()), ("https://github.com/t3dotgg/tstack.git", "", ""));
        let l = link("https://github.com/mattpocock/skills/blob/main/skills/engineering/tdd/SKILL.md").map_err(anyhow::Error::msg)?;
        assert_eq!(l.path, "skills/engineering/tdd/SKILL.md");
        let l = link("git@gitlab.com:team/agent-skills.git").map_err(anyhow::Error::msg)?;
        assert_eq!((l.url.as_str(), l.label.as_str()), ("git@gitlab.com:team/agent-skills.git", "team/agent-skills"));
        for bad in ["", "skills", "github.com/only", "https://github.com/a/b/tree/main/../x", "https://github.com/a/b/issues", "file:///etc"] {
            assert!(link(bad).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn uploads_unpack_from_inside_their_one_folder() -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            z.start_file("perf/bench-report/SKILL.md", o)?;
            z.write_all(b"---\nname: bench-report\n---\n")?;
            z.start_file("perf/bench-report/run.py", o)?;
            z.write_all(b"#!/usr/bin/env python3\n")?;
            z.start_file("__MACOSX/perf/._x", o)?;
            z.write_all(b"junk")?;
            z.start_file("../evil", o)?;
            z.write_all(b"x")?;
            z.finish()?;
        }
        let files = unpack("perf.zip", buf.get_ref()).map_err(anyhow::Error::msg)?;
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["bench-report/SKILL.md", "bench-report/run.py"]);
        assert!(files.iter().all(|f| f.exec));
        let gz = {
            let mut t = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            t.append_data(&mut h, "a/SKILL.md", &b"# a\n"[..])?;
            t.into_inner()?.finish()?
        };
        assert_eq!(unpack("x.tgz", &gz).map_err(anyhow::Error::msg)?.first().map(|f| f.path.clone()), Some("a/SKILL.md".into()));
        assert_eq!(unpack("Notes.md", b"# n").map_err(anyhow::Error::msg)?.len(), 1);
        assert!(unpack("x.exe", b"MZ").is_err());
        Ok(())
    }

    #[test]
    fn written_skills_have_their_frontmatter() -> anyhow::Result<()> {
        let f = written("Change Log", "Use when cutting a release.", "# Steps\n1. Read.").map_err(anyhow::Error::msg)?;
        let text = String::from_utf8_lossy(&f.first().map(|f| f.bytes.clone()).unwrap_or_default()).into_owned();
        assert!(text.starts_with("---\nname: change-log\ndescription: \"Use when cutting a release.\"\n---\n\n# Steps"), "{text}");
        assert!(written("", "x", "y").is_err() && written("x", " ", "y").is_err());
        Ok(())
    }

    #[test]
    fn a_folder_walks_without_git() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        std::fs::create_dir_all(d.path().join(".git"))?;
        std::fs::write(d.path().join(".git/HEAD"), "ref")?;
        std::fs::create_dir_all(d.path().join("s/a"))?;
        std::fs::write(d.path().join("s/a/SKILL.md"), "# a")?;
        let files = walk(d.path())?;
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["s/a/SKILL.md"]);
        Ok(())
    }
}
