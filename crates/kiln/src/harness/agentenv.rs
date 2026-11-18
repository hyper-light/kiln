//! A session's environment (docs/agents.md, "Environments"): a person's named set of variables,
//! as the server resolves it for a session's runner ([`Resolved`]), and what a machine's user has,
//! by name alone ([`MachineVar`]). Decided by the user (2026-11-18).
//!
//! The server keeps environments and opens their secrets; the runner reads `machine` variables from
//! its user's environment as the harness starts and layers the whole over the base
//! (`crate::harness::kinds::set_env`). A secret's value is in [`Resolved`] alone, which its
//! `Debug` never shows, and which the runner keeps in memory only (never in its record of sessions,
//! a move or the session's state).

use serde::{Deserialize, Serialize};

mod places;
#[cfg(windows)]
mod windows;

/// What a session's environment starts from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// The machine's user's environment (docs/agents.md, "Where a harness is found").
    #[default]
    User,
    /// Only what a harness needs to run (docs/agents.md, "What one is").
    Clean,
}

/// Where a variable's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum From {
    /// The machine's user's variable `of` (its own name when none), read on the machine.
    Machine,
    /// A value kept as it is, shown to its owner.
    Value,
    /// The owner's own secret (`secrets.mine`), by name.
    Secret,
    /// Taken away from what the base gives.
    Unset,
}

/// One variable of an environment as its owner wrote it: what the server keeps and the UI shows
/// (a secret by its name, never its value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Var {
    pub name: String,
    pub from: From,
    /// `machine`: the machine's variable it takes, when not its own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub of: Option<String>,
    /// `value`: its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub value: Option<String>,
    /// `secret`: the owner's secret's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub secret: Option<String>,
}

/// One variable as a session's runner gets it: a secret's value opened.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Set {
    /// The machine's user's variable of this name.
    Machine(String),
    /// This value (a `value` variable's, or a secret's).
    Value(String),
    Unset,
}

impl std::fmt::Debug for Set {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Machine(of) => f.debug_tuple("Machine").field(of).finish(),
            // A secret's value may be one: never written out.
            Self::Value(_) => f.write_str("Value(..)"),
            Self::Unset => f.write_str("Unset"),
        }
    }
}

/// A session's environment as its runner gets it (`Spec::agentenv`, `Command::Agentenv`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub base: Base,
    /// In order: a later one of a name wins.
    pub vars: Vec<(String, Set)>,
}

/// Where a machine's user's variable is set (docs/agents.md, "What a machine's user has").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Source {
    LoginShell,
    Launchd,
    SystemdUser,
    Pam,
    UserRegistry,
    SystemRegistry,
    Runner,
}

/// A variable a machine's user has, by name alone, and where it's set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct MachineVar {
    pub name: String,
    pub sources: Vec<Source>,
}

/// What the server asks a runner (`ServerMsg::UserEnv`): its user's variables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {}

/// Its answer (`AgentMsg::Answer`): its user's variables, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Answer {
    pub vars: Vec<MachineVar>,
}

/// This machine's user's variables, by name, and where each is set ([`read`]; kiln's own, `KILN_*`,
/// left out): never a value.
pub async fn answer(_: Ask) -> Answer {
    let user = refresh().await;
    Answer { vars: user.sources.into_iter().map(|(name, sources)| MachineVar { name, sources }).collect() }
}

/// The machine's user's environment as it stands ([`read`]): what base `user` gives a session, and
/// what its `machine` variables are read from, with where each is set. Its values never leave the
/// machine, and nothing writes them out (it has no `Debug`).
#[derive(Clone, Default)]
pub struct UserEnv {
    pub vars: Vec<(String, String)>,
    pub sources: std::collections::BTreeMap<String, Vec<Source>>,
}

/// How long a command that says where variables are set (`launchctl`, `systemctl`) may take.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const PLACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The machine's user's environment now, as a terminal of theirs would start with it. A runner's
/// service (launchd's, systemd's, OpenRC's, a Windows task) keeps the environment it started with and
/// never reads its user's startup files, so this is read each time a session starts:
///
/// - on Unix, the runner's own, under what each place of this system holds now (Linux: PAM's
///   `pam_env` files where a PAM stack loads it, then the systemd user manager where one answers;
///   macOS: launchd's user domain), under what the login shell's startup files set
///   (`crate::reach::serve::login_env`, asked as `$SHELL -l -i`: bash, zsh, fish or sh alike);
/// - on Windows, the runner's own under the registry's as Windows composes them for a new process
///   (`places::compose_windows`), read now: a variable set in System Properties since the runner
///   started never reached it.
pub async fn read() -> UserEnv {
    let runner: Vec<(String, String)> = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
    #[cfg(windows)]
    let composed = match crate::rt::blocking(windows::registry).await.and_then(|r| r.map_err(anyhow::Error::from)) {
        Ok((system, user)) => places::compose_windows(&runner, &system, &user),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "the registry's environment couldn't be read: sessions get the runner's");
            places::compose_windows(&runner, &[], &[])
        }
    };
    #[cfg(not(windows))]
    let composed = {
        let login = crate::reach::serve::login_env().await;
        places::compose_unix(&runner, &unix_places().await, &login)
    };
    let mut sources = composed.sources;
    sources.retain(|name, _| !kilns(name));
    UserEnv { vars: composed.env.vars.into_iter().filter(|(k, _)| !kilns(k)).collect(), sources }
}

/// launchd's user domain: the variables `launchctl setenv` set for what launchd starts in this user's
/// session, named by `launchctl print` (its GUI domain, else its user domain where the runner runs
/// outside one), each read with `launchctl getenv` (launchctl(1)).
#[cfg(target_os = "macos")]
async fn unix_places() -> Vec<(Source, Vec<(String, Option<String>)>)> {
    let launchctl = |args: &[&str]| {
        let mut cmd = std::process::Command::new("/bin/launchctl");
        cmd.args(args);
        crate::proc::output(cmd, PLACE_TIMEOUT)
    };
    let uid = nix::unistd::getuid();
    let mut names: Vec<String> = Vec::new();
    for domain in [format!("gui/{uid}"), format!("user/{uid}")] {
        if let Ok(o) = launchctl(&["print", &domain]).await
            && o.status.success()
        {
            for n in places::launchd_names(&String::from_utf8_lossy(&o.stdout)) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
    }
    let mut set = Vec::new();
    for name in names {
        if let Ok(o) = launchctl(&["getenv", &name]).await
            && o.status.success()
            && let Some(v) = places::launchd_value(&String::from_utf8_lossy(&o.stdout))
        {
            set.push((name, Some(v)));
        }
    }
    vec![(Source::Launchd, set)]
}

/// PAM's files, where a PAM stack of this machine loads `pam_env` (Alpine's logins, without PAM,
/// don't), then the systemd user manager's environment, where one answers (none under OpenRC, in
/// most containers, or in WSL without systemd): `environment.d` and `systemctl --user
/// set-environment` are in it.
#[cfg(target_os = "linux")]
async fn unix_places() -> Vec<(Source, Vec<(String, Option<String>)>)> {
    let pam = crate::rt::blocking(pam_files).await.unwrap_or_default();
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "show-environment"]);
    // A runner that's a system service (OpenRC's, or systemd's with `User=`) has no XDG_RUNTIME_DIR;
    // its user's manager, where one runs, listens in /run/user/<uid> (pam_systemd(8)).
    let runtime = std::path::PathBuf::from(format!("/run/user/{}", nix::unistd::getuid()));
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() && runtime.is_dir() {
        cmd.env("XDG_RUNTIME_DIR", runtime);
    }
    let manager = match crate::proc::output(cmd, PLACE_TIMEOUT).await {
        Ok(o) if o.status.success() => places::systemd_environment(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    };
    vec![(Source::Pam, pam), (Source::SystemdUser, manager)]
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
async fn unix_places() -> Vec<(Source, Vec<(String, Option<String>)>)> {
    Vec::new()
}

/// What pam_env sets, in its order (pam_env(8)): its configuration file, then `/etc/environment`,
/// then the user's `~/.pam_environment` where a stack turns `user_readenv` on; each from the
/// vendor directory where `/etc` has none (`/usr/etc`, openSUSE's). Nothing where no stack in
/// `/etc/pam.d` (or the vendor's `/usr/lib/pam.d`, `/usr/etc/pam.d`) loads it.
#[cfg(target_os = "linux")]
fn pam_files() -> Vec<(String, Option<String>)> {
    let stacks: Vec<String> = ["/etc/pam.d", "/usr/lib/pam.d", "/usr/etc/pam.d"]
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    let (used, user_readenv) = places::pam_env_use(stacks.iter().map(String::as_str));
    if !used {
        return Vec::new();
    }
    let first = |paths: &[&str]| paths.iter().find_map(|p| std::fs::read_to_string(p).ok());
    let home = crate::os::home_dir().to_string_lossy().into_owned();
    let shell = nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten().map(|u| u.shell.to_string_lossy().into_owned()).unwrap_or_default();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    if let Some(conf) = first(&["/etc/security/pam_env.conf", "/usr/etc/security/pam_env.conf"]) {
        out.extend(places::pam_conf(&conf, &[], &home, &shell));
    }
    if let Some(env) = first(&["/etc/environment", "/usr/etc/environment"]) {
        out.extend(places::pam_environment(&env));
    }
    if user_readenv && let Ok(own) = std::fs::read_to_string(std::path::Path::new(&home).join(".pam_environment")) {
        let set: Vec<(String, String)> = out.iter().filter_map(|(k, v)| Some((k.clone(), v.clone()?))).collect();
        out.extend(places::pam_conf(&own, &set, &home, &shell));
    }
    out
}

/// The user's environment as last read ([`refresh`]), kept in memory alone.
static SAID: parking_lot::Mutex<Option<Vec<(String, String)>>> = parking_lot::const_mutex(None);

/// [`read`], kept as what a session's harness starts with ([`said`]): read as each session's harness
/// starts (`crate::harness::session`), and as the server asks which variables there are.
pub async fn refresh() -> UserEnv {
    let user = read().await;
    *SAID.lock() = Some(user.vars.clone());
    user
}

/// The user's environment as [`refresh`] read it last, without reading it again: before it ever did,
/// what the login shell said, else the runner's own.
pub fn said() -> Vec<(String, String)> {
    SAID.lock().clone().or_else(crate::reach::serve::login_said).unwrap_or_else(|| std::env::vars().collect())
}

/// Whether `name` is one of kiln's own variables (`KILN_*`), which come from kiln alone (docs/agents.md,
/// "Where they apply"); on Windows whatever its case, as Windows reads names.
fn kilns(name: &str) -> bool {
    if cfg!(windows) { name.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("KILN_")) } else { name.starts_with("KILN_") }
}

/// Whether two names are one variable's: on Windows whatever their case.
fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// `user`'s value of `name` (its last).
fn lookup<'a>(user: &'a [(String, String)], name: &str) -> Option<&'a str> {
    user.iter().rev().find(|(k, _)| same_name(k, name)).map(|(_, v)| v.as_str())
}

/// What `env`'s variables make of a session's environment, in order (a later one of a name wins): a
/// name set to a value, or taken away (`None`): an `unset` one, and a `machine` one whose variable
/// the machine's user (`user`, [`said`]) hasn't, which is left unset rather than given another
/// value of that name ([`missing`] says which). kiln's own (`KILN_*`) are never set from here.
pub fn layer(env: &Resolved, user: &[(String, String)]) -> Vec<(String, Option<String>)> {
    env.vars
        .iter()
        .filter(|(k, _)| !kilns(k))
        .map(|(k, set)| {
            let v = match set {
                Set::Value(v) => Some(v.clone()),
                Set::Machine(of) => lookup(user, of).map(str::to_owned),
                Set::Unset => None,
            };
            (k.clone(), v)
        })
        .collect()
}

/// `env`'s `machine` variables the machine's user (`user`) hasn't: each its name, and the machine's
/// variable it takes.
pub fn missing(env: &Resolved, user: &[(String, String)]) -> Vec<(String, String)> {
    env.vars.iter().filter_map(|(k, set)| match set {
        Set::Machine(of) if lookup(user, of).is_none() && !kilns(k) => Some((k.clone(), of.clone())),
        _ => None,
    }).collect()
}

/// What base `clean` keeps of the user's environment (docs/agents.md, "What one is"): only what a
/// harness needs to run, from the user's own values.
///
/// - Everywhere, its locale and time zone: `LANG`, `LC_ALL` and each `LC_*` category, and `TZ`
///   (POSIX.1-2017, XBD §8.2, §8.3; glibc's further `LC_*` categories, locale(7)).
/// - On Unix, its home, user, shell, search path and temporary folder: `HOME`, `LOGNAME`, `PATH`,
///   `SHELL`, `TMPDIR` (XBD §8.3), and `USER`, which BSD's and Linux's logins set beside `LOGNAME`
///   (environ(7)). macOS's `TMPDIR` is the user's own folder, which launchd sets.
/// - On Windows, its home (`USERPROFILE`, `HOMEDRIVE`, `HOMEPATH`), user (`USERNAME`, `USERDOMAIN`),
///   shell (`ComSpec`), search path (`Path`, `PATHEXT`), temporary folder (`TMP`, `TEMP`, which
///   `GetTempPath2W` reads in that order), and what Windows' own libraries and programs look for:
///   `SystemRoot` (without which "a side-by-side assembly" won't run, Python's `subprocess.Popen`
///   docs), `SystemDrive`, `windir`, `APPDATA`, `LOCALAPPDATA`, `ProgramData` and `ProgramFiles`
///   (Microsoft Learn, "Recognized environment variables"). Windows keeps its locale and time zone as
///   settings, not variables.
#[cfg(not(windows))]
const CLEAN: &[&str] = &["HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "TZ", "LANG"];
#[cfg(windows)]
const CLEAN: &[&str] = &[
    "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "USERNAME", "USERDOMAIN", "ComSpec", "Path", "PATHEXT", "TMP", "TEMP", "SystemRoot", "SystemDrive", "windir", "APPDATA",
    "LOCALAPPDATA", "ProgramData", "ProgramFiles", "TZ", "LANG",
];

/// [`CLEAN`]'s variables of `user`.
pub fn clean(user: &[(String, String)]) -> Vec<(String, String)> {
    user.iter().filter(|(k, _)| k.starts_with("LC_") || CLEAN.iter().any(|c| same_name(c, k))).cloned().collect()
}

/// Whether `name` is a variable's name an environment may hold: a portable one
/// (`[A-Za-z_][A-Za-z0-9_]*`, POSIX.1-2017 XBD 8.1's portable set, which Windows takes too).
pub fn valid_name(name: &str) -> bool {
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_variables_name_is_a_portable_one() {
        for ok in ["ANTHROPIC_API_KEY", "_x", "a1"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A=B", "É"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    fn user() -> Vec<(String, String)> {
        [("HOME", "/home/u"), ("WORK_KEY", "fake-work"), ("SHELL", "/bin/zsh"), ("LC_TIME", "C"), ("EDITOR", "vi"), ("PATH", "/usr/bin")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn env(vars: Vec<(&str, Set)>) -> Resolved {
        Resolved { id: "e".into(), name: "work".into(), version: 3, base: Base::User, vars: vars.into_iter().map(|(k, v)| (k.to_owned(), v)).collect() }
    }

    /// Its variables in order, a later one of a name winning: a value as it is, a `machine` one read
    /// from the user's, an `unset` one and a `machine` one the user hasn't taken away, kiln's own never.
    #[test]
    fn an_environments_variables_layer_in_order_from_the_users() {
        let e = env(vec![
            ("ANTHROPIC_API_KEY", Set::Machine("WORK_KEY".into())),
            ("EDITOR", Set::Unset),
            ("A", Set::Value("1".into())),
            ("A", Set::Value("2".into())),
            ("GONE", Set::Machine("NOT_ON_THIS_MACHINE".into())),
            ("KILN_DATA_DIR", Set::Value("/production".into())),
        ]);
        let got = layer(&e, &user());
        assert_eq!(
            got,
            vec![("ANTHROPIC_API_KEY".into(), Some("fake-work".into())), ("EDITOR".into(), None), ("A".into(), Some("1".into())), ("A".into(), Some("2".into())), ("GONE".into(), None)]
        );
        assert_eq!(missing(&e, &user()), vec![("GONE".to_owned(), "NOT_ON_THIS_MACHINE".to_owned())]);
    }

    /// Base `clean`: the user's home, shell, search path and locale, and nothing else of theirs.
    #[cfg(not(windows))]
    #[test]
    fn a_clean_base_keeps_only_what_a_harness_needs() {
        let kept: Vec<String> = clean(&user()).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kept, ["HOME", "SHELL", "LC_TIME", "PATH"]);
    }

    #[test]
    fn a_resolved_values_debug_never_shows_it() {
        let r = Resolved { id: "e".into(), name: "n".into(), version: 1, base: Base::User, vars: vec![("K".into(), Set::Value("sk-ant-secret".into())), ("M".into(), Set::Machine("HOME".into()))] };
        let shown = format!("{r:?}");
        assert!(!shown.contains("sk-ant-secret"), "{shown}");
        assert!(shown.contains("HOME"), "{shown}");
    }
}
