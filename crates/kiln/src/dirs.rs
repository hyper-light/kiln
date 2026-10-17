//! Folders on a machine, for the console's folder picker (`GET /api/machines/:target/dirs`): the
//! folders inside one folder, most recently modified first. This server reads its own disk; a
//! machine's runner reads its own when asked ([`crate::proto::ServerMsg::DirList`], sent only to
//! runners that announce [`crate::proto::CAP_DIRS`]).
//!
//! Paths come as people type them: `~` is the home of the user kiln runs as, a relative path is
//! under it, and `.` / `..` are resolved by name (a symlinked folder stays where it was reached,
//! like a shell's `cd`). Paths go back in `~` form when they are under home. One listing reads one
//! folder: nothing is walked into, so no symlink can make it loop.

use std::io::ErrorKind;
use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Most folders one listing returns.
pub const LIMIT: usize = 200;
/// Most entries one listing looks at: a folder of millions of files must not hold a thread.
const SCAN: usize = 20_000;
/// How long the server waits for a machine's listing.
pub const WAIT: Duration = Duration::from_secs(3);

/// The folders in one folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// The folder, normalized (`~` form under home).
    pub path: String,
    /// The same folder, absolute.
    pub abs: String,
    /// The folder above it (`~` form under home); `None` at the root.
    pub parent: Option<String>,
    /// A folder is there. False when nothing is, or something that is not a folder; true when
    /// one is but can't be read (permission denied).
    pub exists: bool,
    /// Its folders (symlinks to folders included), most recently modified first.
    pub dirs: Vec<Dir>,
    /// It has more than [`LIMIT`] folders (or too many entries to look at every one).
    pub truncated: bool,
    /// Why it can't be listed: `no such folder`, `not a folder`, `permission denied`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dir {
    pub name: String,
    /// Last modified, unix seconds (0: unknown).
    pub modified: i64,
}

/// The folders in the folder `typed` names, `home` being `~`. Hidden folders (a leading `.`) are
/// left out unless the last segment typed starts with `.` (`~/.` lists home with them, and so
/// does a hidden folder itself, `~/.config`). Blocking.
pub fn list(typed: &str, home: &Path) -> Listing {
    let typed = typed.trim();
    let home = std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf());
    let abs = resolve(typed, &home);
    let mut out = Listing {
        path: tilde(&abs, &home),
        abs: abs.to_string_lossy().into_owned(),
        parent: abs.parent().map(|p| tilde(p, &home)),
        ..Listing::default()
    };
    match std::fs::metadata(&abs) {
        Ok(m) if m.is_dir() => out.exists = true,
        Ok(_) => {
            out.error = Some("not a folder".to_owned());
            return out;
        }
        Err(e) => {
            out.exists = e.kind() == ErrorKind::PermissionDenied;
            out.error = Some(problem(&e));
            return out;
        }
    }
    let entries = match std::fs::read_dir(&abs) {
        Ok(rd) => rd,
        Err(e) => {
            out.error = Some(problem(&e));
            return out;
        }
    };
    let last = typed.trim_end_matches(is_sep).rsplit(is_sep).next().unwrap_or_default();
    let hidden = last.starts_with('.') && last != "..";
    let mut dirs = Vec::new();
    for (n, entry) in entries.enumerate() {
        if n >= SCAN {
            out.truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        // A name that isn't text couldn't be typed back.
        let Ok(name) = entry.file_name().into_string() else { continue };
        if name.starts_with('.') && !hidden {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        // A symlink counts when it leads to a folder: looked up once (a loop is an error), never
        // walked into.
        let meta = if kind.is_dir() {
            entry.metadata()
        } else if kind.is_symlink() {
            std::fs::metadata(entry.path())
        } else {
            continue;
        };
        let Ok(meta) = meta else { continue };
        if meta.is_dir() {
            dirs.push(Dir { name, modified: modified(&meta) });
        }
    }
    dirs.sort_unstable_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    if dirs.len() > LIMIT {
        dirs.truncate(LIMIT);
        out.truncated = true;
    }
    out.dirs = dirs;
    out
}

/// [`list`] on this machine (home: the user kiln runs as), on a blocking thread.
pub async fn here(typed: String) -> Listing {
    let path = typed.clone();
    crate::rt::blocking(move || list(&typed, &crate::os::home_dir()))
        .await
        .unwrap_or_else(|e| Listing { path, error: Some(format!("{e:#}")), ..Listing::default() })
}

/// A runner's answer to [`crate::proto::ServerMsg::DirList`]: [`here`] as JSON.
pub async fn answer(typed: String) -> String {
    serde_json::to_string(&here(typed).await).unwrap_or_default()
}

fn is_sep(c: char) -> bool {
    c == '/' || c == MAIN_SEPARATOR
}

/// What a typed path names: `~` (or nothing) is home, a relative path is under it, `.` and `..`
/// are resolved by name.
fn resolve(typed: &str, home: &Path) -> PathBuf {
    let raw = match typed {
        "" | "~" => home.to_path_buf(),
        t => match t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
            Some(rest) => home.join(rest),
            // An absolute path replaces home.
            None => home.join(t),
        },
    };
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::CurDir => {}
            // Above the root is the root.
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(c),
        }
    }
    out
}

/// A path as people read it: `~/...` under home (unless home is the root).
fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if home.parent().is_some() => {
            if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~{MAIN_SEPARATOR}{}", rest.display())
            }
        }
        _ => p.to_string_lossy().into_owned(),
    }
}

fn problem(e: &std::io::Error) -> String {
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory => "no such folder".to_owned(),
        ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => e.to_string(),
    }
}

fn modified(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;

    /// A folder whose last-modified time is `secs` after the epoch.
    fn folder(at: &Path, secs: u64) -> anyhow::Result<()> {
        std::fs::create_dir_all(at)?;
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        File::open(at)?.set_times(FileTimes::new().set_modified(t))?;
        Ok(())
    }

    fn names(l: &Listing) -> Vec<&str> {
        l.dirs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn folders_most_recent_first_without_files_or_hidden_ones() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let p = home.path().join("Projects");
        folder(&p.join("old"), 1_000)?;
        folder(&p.join("new"), 3_000)?;
        folder(&p.join("b-tie"), 2_000)?;
        folder(&p.join("a-tie"), 2_000)?;
        folder(&p.join(".git"), 9_000)?;
        std::fs::write(p.join("notes.txt"), "x")?;
        let l = list("~/Projects", home.path());
        assert_eq!(names(&l), ["new", "a-tie", "b-tie", "old"]);
        assert_eq!(l.dirs.first().map(|d| d.modified), Some(3_000));
        assert!(l.exists && !l.truncated && l.error.is_none());
        assert_eq!(l.path, format!("~{MAIN_SEPARATOR}Projects"));
        assert_eq!(l.abs, p.to_string_lossy());
        assert_eq!(l.parent.as_deref(), Some("~"));
        Ok(())
    }

    #[test]
    fn hidden_folders_when_the_last_segment_typed_starts_with_a_dot() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        folder(&home.path().join(".config/.nested"), 5)?;
        folder(&home.path().join(".config/visible"), 6)?;
        folder(&home.path().join("code"), 4)?;
        assert_eq!(names(&list("~", home.path())), ["code"]);
        // `~/.`: home, with its hidden folders.
        let l = list("~/.", home.path());
        assert_eq!(l.path, "~");
        assert_eq!(names(&l), [".config", "code"]);
        // Inside a hidden folder, its hidden folders show too.
        assert_eq!(names(&list("~/.config/", home.path())), ["visible", ".nested"]);
        // `..` goes up; it doesn't ask for hidden folders.
        assert_eq!(names(&list("~/code/..", home.path())), ["code"]);
        Ok(())
    }

    #[test]
    fn at_most_the_limit_newest_first() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        for i in 0..(LIMIT as u64 + 5) {
            folder(&home.path().join(format!("d{i:03}")), 10_000 + i)?;
        }
        let l = list("", home.path());
        assert_eq!(l.dirs.len(), LIMIT);
        assert!(l.truncated);
        assert_eq!(l.dirs.first().map(|d| d.name.as_str()), Some("d204"));
        assert_eq!(l.dirs.last().map(|d| d.name.as_str()), Some("d005"));
        Ok(())
    }

    #[test]
    fn home_relative_and_absolute_paths_come_back_in_tilde_form() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        folder(&h.join("Projects/kiln"), 1)?;
        let sep = MAIN_SEPARATOR;
        let kiln = format!("~{sep}Projects{sep}kiln");
        for typed in ["~/Projects/kiln", "Projects/kiln", " ~/Projects/./x/../kiln/ "] {
            assert_eq!(list(typed, h).path, kiln, "{typed}");
        }
        assert_eq!(list(&h.join("Projects/kiln").to_string_lossy(), h).path, kiln);
        let l = list("~", h);
        assert_eq!((l.path.as_str(), l.abs.as_str()), ("~", &*h.to_string_lossy()));
        // Above home is absolute.
        let up = h.parent().map(|p| p.to_string_lossy().into_owned());
        assert_eq!(l.parent, up);
        assert_eq!(list("~/..", h).path, up.unwrap_or_default());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_root_has_no_parent_and_nothing_climbs_above_it() {
        let l = list("/../..", Path::new("/nonexistent-home"));
        assert_eq!((l.path.as_str(), l.parent.as_deref()), ("/", None));
        assert!(l.exists);
        // Home at the root has no `~` form.
        assert_eq!(list("/tmp", Path::new("/")).path, "/tmp");
    }

    #[test]
    fn what_cant_be_listed_says_why() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        std::fs::write(home.path().join("file"), "x")?;
        let l = list("~/missing", home.path());
        assert_eq!((l.exists, l.error.as_deref()), (false, Some("no such folder")));
        assert_eq!(l.path, format!("~{MAIN_SEPARATOR}missing"));
        assert_eq!(l.parent.as_deref(), Some("~"));
        let l = list("~/file", home.path());
        assert_eq!((l.exists, l.error.as_deref()), (false, Some("not a folder")));
        let l = list("~/file/below", home.path());
        assert_eq!((l.exists, l.error.as_deref()), (false, Some("no such folder")));
        assert!(l.dirs.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folders_exist_but_say_permission_denied() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir()?;
        let locked = home.path().join("locked");
        folder(&locked.join("inside"), 1)?;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;
        let l = list("~/locked", home.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
        // Root reads anything.
        if nix::unistd::geteuid().is_root() {
            return Ok(());
        }
        assert_eq!((l.exists, l.error.as_deref()), (true, Some("permission denied")));
        assert!(l.dirs.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_folders_count_and_loops_are_skipped() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir()?;
        let h = home.path();
        folder(&h.join("real"), 1)?;
        std::fs::write(h.join("file"), "x")?;
        symlink(h.join("real"), h.join("link"))?;
        symlink(h.join("file"), h.join("to-file"))?;
        symlink(h.join("loop"), h.join("loop"))?;
        symlink(h.join("gone"), h.join("dangling"))?;
        let l = list("~", h);
        let mut got = names(&l);
        got.sort_unstable();
        assert_eq!(got, ["link", "real"]);
        // A symlinked folder keeps the path it was reached by.
        let l = list("~/link/", h);
        assert_eq!((l.path.as_str(), l.exists), ("~/link", true));
        Ok(())
    }

    #[test]
    fn listings_cross_the_wire_as_json() -> anyhow::Result<()> {
        let l = Listing {
            path: "~/x".into(),
            abs: "/home/u/x".into(),
            parent: Some("~".into()),
            exists: true,
            dirs: vec![Dir { name: "kiln".into(), modified: 7 }],
            truncated: false,
            error: None,
        };
        let json = serde_json::to_string(&l)?;
        assert!(!json.contains("error"), "no error, no field: {json}");
        assert_eq!(serde_json::from_str::<Listing>(&json)?, l);
        Ok(())
    }
}
