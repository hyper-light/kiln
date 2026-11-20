//! A Windows folder's place, judged from its text as Windows judges it, on any host (docs/agentic.md, "W8 on Linux KVM
//! and Windows WHP", piece 2; UI goal 88, kiln goal part 01a1f1249c5e75b5bc82c9eb011550f4). An isolated session on a
//! Windows runner sees its project at its path as WSL names it (`/mnt/c/…`, Shards' D235) in its microVM
//! (`super::isolated::guest_path`), and its
//! harness is told each folder of the machine's beneath that project as the microVM sees it (`super::isolated::Seen`),
//! its working folder first (`super::isolated::served_project`). Where its workspace is a volume laid over a
//! repository, the project's top is git's (`git rev-parse --show-toplevel`), which Git for Windows resolves on its disk
//! and writes with `/` (`C:/Users/ada/proj`), while the session's folder is spelled as it was given (`C:\Users\ada\proj`,
//! `c:\users\ada\proj`, `\\?\C:\…` from `std::fs::canonicalize`). Matched as text the two never met, so the harness was
//! told the machine's spelling, which names nothing in its microVM. Here a path lies in a folder wherever Windows would
//! open it beneath that folder by their spellings:
//!
//! - **Prefixes**, stripped as Git for Windows' own `normalize_ntpath` strips them (git-for-windows/git
//!   `compat/mingw.c`, main e766978a9db5 of 2026-11-13, which its `mingw_strbuf_realpath` and `mingw_getcwd` apply to
//!   what `GetFinalPathNameByHandleW` answers): the NT namespace's `\??\` and `\DosDevices\`, the Win32 file
//!   namespace's `\\?\`, and `UNC\` after one of them, which names a share as `\\server\share` does; and the device
//!   namespace's `\\.\`, which names the same files (Microsoft Learn, "File path formats on Windows systems",
//!   2025-10-22: `\\.\C:\Test\Foo.txt` and `\\?\C:\Test\Foo.txt`; a device path begins "with two separators and a
//!   question mark or period").
//! - **Separators**: `/` and `\` alike, and a run of them one ("All forward slashes (/) are converted into the standard
//!   Windows separator, the back slash (\)", and "a series of slashes that follow the first two slashes are collapsed
//!   into a single slash", the same page).
//! - **Relative components**: `.` dropped, and `..` taking back the name before it, never past the root (the same
//!   page, "Evaluate relative components").
//! - **Case**: a drive's letter, a share's server and name, a device's name and every folder's name compared without
//!   case ("path and directory names are case-insensitive", the same page): each character by its simple uppercase, a
//!   character past the Basic Multilingual Plane as it is, as Windows' ordinal comparison maps each UTF-16 unit through
//!   "the operating system uppercase table" (Microsoft Learn, `CompareStringOrdinal`'s `bIgnoreCase`, 2018-12-05).
//!
//! What its text can't place lies in no folder: a relative path, one rooted on the current drive (`\x`) or relative to
//! a drive's own folder (`C:x`), and a place a short 8.3 name (`ADALUN~1`) or a link spells, which Windows resolves on
//! its disk, as git resolves its top. A verbatim path (`\\?\`), which Windows hands its file system unnormalized, is
//! judged as the others are: Windows reserves `/`, and gives `.` and `..` as a path's components their own meanings
//! (Microsoft Learn, "Naming Files, Paths, and Namespaces", 2024-08-28), so no folder is named by them.

/// The last code point of Unicode's Basic Multilingual Plane: the last a single UTF-16 unit writes, and so the last
/// Windows' uppercase table maps (module docs).
const BMP_LAST: u32 = 0xFFFF;

/// The name the NT namespace gives its folder of DOS devices, which a path may begin `\DosDevices\` by.
const DOS_DEVICES: &str = "DosDevices";

/// The name a device path gives a share after its prefix (`\\?\UNC\server\share`).
const UNC: &str = "UNC";

/// Where a Windows path is rooted, as its text says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root<'a> {
    /// A drive, by its letter.
    Drive(char),
    /// A share, by its server's name and its own.
    Share(&'a str, &'a str),
    /// Another device, by its name: a volume's (`Volume{…}`), a pipe's namespace's.
    Device(&'a str),
}

impl Root<'_> {
    /// Whether `self` and `other` root one place, as Windows names them: without case.
    fn same(&self, other: &Root<'_>) -> bool {
        match (self, other) {
            (Root::Drive(letter), Root::Drive(its_letter)) => letter.eq_ignore_ascii_case(its_letter),
            (Root::Share(server, share), Root::Share(its_server, its_share)) => same_name(server, its_server) && same_name(share, its_share),
            (Root::Device(name), Root::Device(its_name)) => same_name(name, its_name),
            _ => false,
        }
    }
}

/// The names of `path`'s place beneath folder `top`, in order and in `path`'s own spelling, where Windows would open
/// `path` beneath `top` by their spellings (module docs): none where it lies elsewhere, or where the text of either
/// can't place it.
pub fn beneath<'a>(path: &'a str, top: &str) -> Option<Vec<&'a str>> {
    let (root, names) = placed(path)?;
    let (top_root, top_names) = placed(top)?;
    let within = root.same(&top_root) && names.len() >= top_names.len() && names.iter().zip(&top_names).all(|(name, top_name)| same_name(name, top_name));
    within.then(|| names.into_iter().skip(top_names.len()).collect())
}

/// Whether `c` separates a Windows path's names: `\`, or `/`, which Windows takes for it.
fn separator(c: char) -> bool {
    c == '\\' || c == '/'
}

/// A Windows path as its text places it: its root, and its folders' names in order, `.` dropped and `..` taking back
/// the name before it; none for a path its text can't place (module docs).
fn placed(path: &str) -> Option<(Root<'_>, Vec<&str>)> {
    let (rest, prefixed) = unprefixed(path);
    if let Some((letter, after)) = drive(rest) {
        return Some((Root::Drive(letter), names(after)));
    }
    // Past a device prefix: a share (`UNC\server\share`), else another device by its name. Without one, two
    // separators begin a share (`\\server\share`).
    let (share, rest) = if prefixed {
        let (first, after) = rest.split_once(separator).unwrap_or((rest, ""));
        if !first.eq_ignore_ascii_case(UNC) {
            return named(first).then(|| (Root::Device(first), names(after)));
        }
        (true, after)
    } else {
        match rest.strip_prefix(separator).and_then(|after| after.strip_prefix(separator)) {
            Some(after) => (true, after),
            None => (false, rest),
        }
    };
    if !share {
        return None;
    }
    let (server, after) = rest.split_once(separator).unwrap_or((rest, ""));
    let (name, after) = after.split_once(separator).unwrap_or((after, ""));
    (named(server) && named(name)).then(|| (Root::Share(server, name), names(after)))
}

/// `path` past its device or namespace prefix (`\\?\`, `\\.\`, `\??\`, `\DosDevices\`, either separator), and whether
/// it had one; as it is otherwise.
fn unprefixed(path: &str) -> (&str, bool) {
    let bytes = path.as_bytes();
    let separated = |at: usize| bytes.get(at).is_some_and(|b| *b == b'\\' || *b == b'/');
    let device = separated(0) && separated(1) && matches!(bytes.get(2), Some(b'?' | b'.')) && separated(3);
    let nt = separated(0) && bytes.get(1) == Some(&b'?') && bytes.get(2) == Some(&b'?') && separated(3);
    if device || nt {
        return (path.get(4..).unwrap_or_default(), true);
    }
    let dos_devices = separated(0) && path.get(1..=DOS_DEVICES.len()).is_some_and(|name| name.eq_ignore_ascii_case(DOS_DEVICES)) && separated(DOS_DEVICES.len() + 1);
    if dos_devices {
        return (path.get(DOS_DEVICES.len() + 2..).unwrap_or_default(), true);
    }
    (path, false)
}

/// A drive's letter, and what follows its root's separator, where `text` begins `X:\` or `X:/`.
fn drive(text: &str) -> Option<(char, &str)> {
    let mut chars = text.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':') && chars.next().is_some_and(separator)).then(|| (letter, text.get(3..).unwrap_or_default()))
}

/// Whether `name` names a server, a share or a device: neither empty nor a relative component.
fn named(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".."
}

/// `text`'s names in order, either separator between them, `.` dropped and `..` taking back the name before it, never
/// past the root (module docs).
fn names(text: &str) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    for name in text.split(separator) {
        match name {
            "" | "." => {}
            ".." => names.truncate(names.len().saturating_sub(1)),
            name => names.push(name),
        }
    }
    names
}

/// Whether `a` and `b` are one name to Windows: equal, each character taken by its simple uppercase (module docs).
fn same_name(a: &str, b: &str) -> bool {
    a.chars().map(upper).eq(b.chars().map(upper))
}

/// `c` as Windows' uppercase table maps the UTF-16 unit that writes it: its simple uppercase, where that is one
/// character of the Basic Multilingual Plane; else itself, as a character past that plane is, whose two units have no
/// case there (module docs).
fn upper(c: char) -> char {
    if u32::from(c) > BMP_LAST {
        return c;
    }
    let mut mapped = c.to_uppercase();
    match (mapped.next(), mapped.next()) {
        (Some(one), None) if u32::from(one) <= BMP_LAST => one,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::beneath;
    use crate::harness::session::{Isolation, Spec};
    use crate::sandbox::isolated::{Seen, harness_cwd, said, served_project};

    /// UI goal 88, kiln goal part 01a1f1249c5e75b5bc82c9eb011550f4 (docs/agentic.md, "W8 on Linux KVM and Windows WHP",
    /// piece 2): a Windows path lies in a folder wherever Windows would open it there by their spellings, judged from
    /// their text on any host. Do judge, against a top as Git for Windows writes one (`C:/Users/ada/proj`), a share's
    /// and a volume's, paths spelled every way Windows takes them: backslashes, a drive's letter and names in another
    /// case, `\\?\`, `//?/`, `\??\`, `\\.\` and `\DosDevices\` prefixes, `UNC\` after one, runs of separators and a
    /// trailing one, `.` and `..`, a name's non-ASCII case; and paths Windows would not open there, or whose text can't
    /// place them. Expect the names of each one's place beneath the top in its own spelling, and none for the rest.
    #[test]
    fn w8_p2_a_windows_path_lies_in_a_folder_wherever_windows_would_open_it_there() {
        let git = "C:/Users/ada/proj";
        let share = r"\\server\share\repo";
        let volume = r"\\.\volume{B75E2C83-0000-0000-0000-602F00000000}\Test";
        let within: &[(&str, &str, &[&str])] = &[
            (r"C:\Users\ada\proj", git, &[]),
            (r"c:\users\ADA\Proj\src\main.rs", git, &["src", "main.rs"]),
            (r"\\?\C:\Users\ada\proj\crates\kiln", git, &["crates", "kiln"]),
            ("//?/C:/Users/ada/proj/docs", git, &["docs"]),
            (r"\??\C:\Users\ada\proj\docs", git, &["docs"]),
            (r"\\.\C:\Users\ada\proj\docs", git, &["docs"]),
            (r"\DosDevices\c:\Users\ada\proj\docs", git, &["docs"]),
            (r"C:\\Users\ada\\\proj\docs\", git, &["docs"]),
            (r"C:\Users\ada\proj\crates\..\docs\.\x", git, &["docs", "x"]),
            (r"C:\..\..\Users\ada\proj", git, &[]),
            ("C:/Users/Ölaf/Proj/x", "c:/users/ölaf/proj", &["x"]),
            (r"\\SERVER\Share\repo\x", share, &["x"]),
            ("//server/share/repo/x", share, &["x"]),
            (r"\\?\UNC\server\share\repo\x", share, &["x"]),
            (r"\??\unc\Server\SHARE\repo", share, &[]),
            (r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\Test\x", volume, &["x"]),
        ];
        for (path, top, names) in within {
            assert_eq!(beneath(path, top).as_deref(), Some(*names), "{path} beneath {top}");
        }
        let elsewhere: &[(&str, &str)] = &[
            (r"C:\Users\ada\project\x", git),
            (r"D:\Users\ada\proj\x", git),
            (r"C:\Users\ada\proj\..\other", git),
            (r"C:\Users\ada", git),
            (r"\Users\ada\proj", git),
            (r"C:Users\ada\proj", git),
            (r"Users\ada\proj", git),
            ("/work/proj/src", git),
            (r"\\?\C:", git),
            ("C:/x/straße", "C:/X/STRASSE"),
            (r"\\server\other\repo", share),
            (r"\\?\UNC\server", share),
            (r"\\.\pipe\repo", share),
        ];
        for (path, top) in elsewhere {
            assert_eq!(beneath(path, top), None, "{path} beneath {top}");
        }
    }

    /// UI goal 88, kiln goal part 01a1f1249c5e75b5bc82c9eb011550f4 (docs/agentic.md, "W8 on Linux KVM and Windows WHP",
    /// piece 2): on a Windows runner an isolated session whose workspace is a volume Slates serves its microVM, laid over
    /// a repository whose top Git for Windows writes `C:/Users/ada/proj`, works in `C:\Users\ada\proj\crates\kiln`; its
    /// harness is told that folder, and each place beneath its project however Windows spells it, as its microVM sees
    /// it. Do take its served project and its working folder, the folder its harness is told, and what it's told of
    /// paths spelled each way; and a session working on a share. Expect its volume served where WSL names the folder it
    /// lies over (`/mnt/c/Users/ada/proj`, `guest_path`, Shards' D235) and its working folder
    /// `/mnt/c/Users/ada/proj/crates/kiln`, which its harness is told; each spelling of a place beneath the project said
    /// at that place; each path elsewhere said as it is; and a share, on no drive, seen nowhere in a microVM.
    #[test]
    fn w8_p2_a_windows_projects_folders_are_told_as_its_microvm_sees_them_however_windows_spells_them() -> anyhow::Result<()> {
        let cwd = r"C:\Users\ada\proj\crates\kiln";
        let made = crate::workspace::slates::Made { volume: "0123456789abcdef0123456789abcdef".into(), name: "kiln-x".into(), mount: String::new(), ..Default::default() };
        let workspace = crate::workspace::Workspace { kind: "volume".into(), over: Some("C:/Users/ada/proj".into()), sub: "crates/kiln".into(), made: Some(made), ..Default::default() };
        let spec = Spec { kind: "codex".into(), cwd: Some(cwd.into()), isolation: Some(Isolation::microvm()), workspace: Some(workspace), ..Spec::default() };
        assert_eq!(harness_cwd(&spec), "/mnt/c/Users/ada/proj/crates/kiln", "its harness told it works in its folder's place in its microVM");
        let seen = Seen::of(&spec);
        for (path, told) in [
            (r"C:\Users\ada\proj", "/mnt/c/Users/ada/proj"),
            (r"c:\users\ADA\Proj\src\main.rs", "/mnt/c/Users/ada/proj/src/main.rs"),
            (r"\\?\C:\Users\ada\proj\crates\kiln", "/mnt/c/Users/ada/proj/crates/kiln"),
            (r"\??\C:\Users\ada\proj\docs\", "/mnt/c/Users/ada/proj/docs"),
            (r"C:\Users\ada\proj\crates\..\docs", "/mnt/c/Users/ada/proj/docs"),
            (r"C:\Users\ada\project\x", r"C:\Users\ada\project\x"),
            (r"D:\Users\ada\proj\x", r"D:\Users\ada\proj\x"),
            ("/mnt/c/Users/ada/proj/src", "/mnt/c/Users/ada/proj/src"),
        ] {
            assert_eq!(said(seen.as_ref(), path), told, "{path}");
        }
        let harness = kiln_seams::sandbox::Member("codex".into());
        let (served, folder) = served_project(&spec, cwd, &harness).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("its workspace's volume isn't served"))?;
        assert_eq!(served.at, "/mnt/c/Users/ada/proj", "its volume, where its microVM sees the folder it lies over");
        assert_eq!(folder, "/mnt/c/Users/ada/proj/crates/kiln", "its working folder, at the same place in it");
        // A session working on a share, which is on no drive until one is mapped to it: its microVM sees it nowhere
        // (`guest_path` refuses it, and its launch with it), so its harness is told nothing of it.
        let on_share = Spec { kind: "codex".into(), cwd: Some(r"\\server\share\repo".into()), isolation: Some(Isolation::microvm()), ..Spec::default() };
        assert_eq!(Seen::of(&on_share), None, "a share is on no drive its microVM sees");
        Ok(())
    }
}
