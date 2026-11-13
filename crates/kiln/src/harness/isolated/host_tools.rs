//! Tools of a host's own system, which a harness in a microVM can't run (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 18). A microVM runs Linux whatever its machine is, so a
//! project built with Xcode (`xcodebuild` runs on macOS alone: Apple's Xcode system requirements,
//! developer.apple.com/xcode) or with MSVC, Visual C++'s compiler and its MSBuild projects
//! (Windows alone: learn.microsoft.com, "Use the Microsoft C++ toolset from the command line"),
//! can't be built or tested from one. Such a session stays a process on a machine that has them;
//! where it's asked to run isolated, it's refused, saying which tools and why, never moved out of
//! its microVM unasked.
//!
//! What a project needs is read from its own files, as git tracks them: an Xcode project or
//! workspace (`*.xcodeproj/project.pbxproj`, `*.xcworkspace/contents.xcworkspacedata`, the file
//! each bundle keeps), or a Visual C++ project (`*.vcxproj`). A `.sln` alone says nothing: .NET's
//! own builds it on Linux (`dotnet build`).

use std::path::Path;

use super::Refusal;

/// A host system's own toolchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostTool {
    Xcode,
    Msvc,
}

impl HostTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Xcode => "Xcode",
            Self::Msvc => "MSVC (Visual C++)",
        }
    }

    /// The one system it runs on.
    pub fn system(self) -> &'static str {
        match self {
            Self::Xcode => "macOS",
            Self::Msvc => "Windows",
        }
    }

    /// Whether `path`, one of a project's tracked files, says it builds with this.
    fn says(self, path: &str) -> bool {
        let lower = path.to_ascii_lowercase();
        let in_bundle = |bundle: &str, file: &str| lower.strip_suffix(file).and_then(|dir| dir.strip_suffix('/')).is_some_and(|dir| dir.ends_with(bundle));
        match self {
            Self::Xcode => in_bundle(".xcodeproj", "project.pbxproj") || in_bundle(".xcworkspace", "contents.xcworkspacedata"),
            Self::Msvc => lower.ends_with(".vcxproj"),
        }
    }
}

/// `git ls-files` pathspecs of the files [`needed`] reads, at any depth.
pub const PATHSPECS: &[&str] = &[":(glob,icase)**/*.xcodeproj/project.pbxproj", ":(glob,icase)**/*.xcworkspace/contents.xcworkspacedata", ":(glob,icase)**/*.vcxproj"];

/// The host tools a project's `files` (paths from its root) say it builds with, each once.
pub fn needed<'a>(files: impl IntoIterator<Item = &'a str>) -> Vec<HostTool> {
    let mut out = Vec::new();
    for f in files {
        for t in [HostTool::Xcode, HostTool::Msvc] {
            if t.says(f) && !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out.sort();
    out
}

/// Whether a session whose project's files are `files` can run isolated: refused, typed, when its
/// project builds with a tool only its host's own system has.
pub fn check<'a>(files: impl IntoIterator<Item = &'a str>) -> Result<(), Refusal> {
    let tools = needed(files);
    if tools.is_empty() { Ok(()) } else { Err(Refusal::HostTools(tools)) }
}

/// The files of the project at `dir` that [`needed`] reads, as its git tracks them (blocking). A
/// folder git doesn't track says nothing.
pub fn tracked(dir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["ls-files", "-z", "--"]).args(PATHSPECS).env("GIT_TERMINAL_PROMPT", "0").output();
    match out {
        Ok(o) if o.status.success() => o.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect(),
        _ => Vec::new(),
    }
}

/// The refusal's words: each tool, the one system it runs on, and what the person can do.
pub(super) fn refusal_words(tools: &[HostTool]) -> String {
    let each: Vec<String> = tools.iter().map(|t| format!("{}, which runs on {} alone", t.name(), t.system())).collect();
    format!("its project builds with {}: a microVM runs Linux, so its harness couldn't build or test it there. Start it as a process on a machine that has them instead", each.join(", and with "))
}

#[cfg(test)]
mod tests {
    use super::{HostTool, check, needed, tracked};
    use crate::harness::isolated::Refusal;

    /// Xcode's and Visual C++'s projects say so at any depth, whatever their case; a solution, a
    /// Swift package or a C# project says nothing.
    #[test]
    fn step18_a_projects_own_files_say_which_host_tools_it_needs() {
        assert_eq!(needed(["App.xcodeproj/project.pbxproj"]), vec![HostTool::Xcode]);
        assert_eq!(needed(["ios/Runner.xcworkspace/contents.xcworkspacedata"]), vec![HostTool::Xcode]);
        assert_eq!(needed(["native/win/Engine.VCXPROJ", "src/main.rs"]), vec![HostTool::Msvc]);
        assert_eq!(needed(["a/App.xcodeproj/project.pbxproj", "b/Lib.vcxproj", "c/App.xcodeproj/project.pbxproj"]), vec![HostTool::Xcode, HostTool::Msvc]);
        for none in ["App.sln", "Package.swift", "src/App.csproj", "project.pbxproj", "notes/App.xcodeproj.md", "x.xcodeproj/other.pbxproj"] {
            assert_eq!(needed([none]), vec![], "{none}");
        }
    }

    /// Refused where it would run isolated, saying which tools, on which systems, and what to do.
    #[test]
    fn step18_a_session_that_needs_host_tools_is_refused_isolation_saying_why() {
        assert_eq!(check(["src/main.rs"]), Ok(()));
        let r = check(["App.xcodeproj/project.pbxproj", "Lib.vcxproj"]);
        assert_eq!(r, Err(Refusal::HostTools(vec![HostTool::Xcode, HostTool::Msvc])));
        let words = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(words.contains("Xcode, which runs on macOS alone") && words.contains("MSVC (Visual C++), which runs on Windows alone") && words.contains("a process on a machine that has them"), "{words}");
    }

    /// git finds them however deep, and a folder git doesn't track says nothing.
    #[test]
    fn step18_git_finds_a_projects_host_tool_files() -> anyhow::Result<()> {
        let t = tempfile::tempdir()?;
        let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(t.path()).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output();
        anyhow::ensure!(tracked(t.path()).is_empty());
        anyhow::ensure!(git(&["init", "-q"])?.status.success());
        std::fs::create_dir_all(t.path().join("ios/App.xcodeproj"))?;
        std::fs::write(t.path().join("ios/App.xcodeproj/project.pbxproj"), "// !$*UTF8*$!\n")?;
        std::fs::write(t.path().join("README"), "x\n")?;
        anyhow::ensure!(git(&["add", "-A"])?.status.success());
        let files = tracked(t.path());
        anyhow::ensure!(files == ["ios/App.xcodeproj/project.pbxproj"], "{files:?}");
        anyhow::ensure!(needed(files.iter().map(String::as_str)) == [HostTool::Xcode]);
        Ok(())
    }
}
