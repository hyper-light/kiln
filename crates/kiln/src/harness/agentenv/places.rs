//! Where a machine's user's variables are set (docs/agents.md, "What a machine's user has"), read from
//! what each place says: pure functions of its text, so each is tested on every system. Reading the
//! places themselves (a command, a file, the registry) is the parent's, behind its system's `cfg`: what
//! one system reads alone is dead code on the others (`allow(dead_code)` there), tested everywhere.

use super::Source;

/// An environment being made: its variables in the order they were first set, a name's later value
/// taking its place. `fold`: names that differ only by case are one (Windows: "the name of an
/// environment variable [is] case-insensitive", Microsoft Learn, "Environment Variables",
/// learn.microsoft.com/windows/win32/procthread/environment-variables).
#[derive(Clone, Default)]
pub struct Env {
    pub vars: Vec<(String, String)>,
    fold: bool,
}

impl Env {
    pub fn new(fold: bool) -> Self {
        Self { vars: Vec::new(), fold }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn same(&self, a: &str, b: &str) -> bool {
        if self.fold { a.eq_ignore_ascii_case(b) } else { a == b }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| self.same(k, name)).map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, name: &str, value: String) {
        let fold = self.fold;
        match self.vars.iter_mut().find(|(k, _)| if fold { k.eq_ignore_ascii_case(name) } else { k == name }) {
            Some((_, v)) => *v = value,
            None => self.vars.push((name.to_owned(), value)),
        }
    }

    #[cfg_attr(windows, allow(dead_code))]
    pub fn remove(&mut self, name: &str) {
        let fold = self.fold;
        self.vars.retain(|(k, _)| if fold { !k.eq_ignore_ascii_case(name) } else { k != name });
    }
}

/// The user's environment, and where each of its variables is set, by name.
#[derive(Clone, Default)]
pub struct Composed {
    pub env: Env,
    pub sources: std::collections::BTreeMap<String, Vec<Source>>,
}

impl Composed {
    fn from_runner(runner: &[(String, String)], fold: bool) -> Self {
        let mut c = Self { env: Env::new(fold), sources: std::collections::BTreeMap::new() };
        for (k, v) in runner {
            c.set(Source::Runner, k, v.clone());
        }
        c
    }

    fn set(&mut self, source: Source, name: &str, value: String) {
        self.env.set(name, value);
        self.said(source, name);
    }

    /// `name` is set at `source`, whatever its value there.
    fn said(&mut self, source: Source, name: &str) {
        // Under its name as first set (Windows: `Path` and `PATH` are one).
        let key = if self.env.fold { self.sources.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned().unwrap_or_else(|| name.to_owned()) } else { name.to_owned() };
        let sources = self.sources.entry(key).or_default();
        if !sources.contains(&source) {
            sources.push(source);
            sources.sort();
        }
    }
}

/// A Unix user's environment as a terminal of theirs would have it now: the runner's own; over it,
/// each of `places` as it stands (in order: PAM's files, then the systemd user manager or launchd,
/// each of which the runner read only as it started, if ever); over those, what the login shell's
/// startup files set (`login`, its answer: a variable it has otherwise than the runner, which it
/// inherited from). `None` in a place takes a variable away (pam_env's bare name).
#[cfg_attr(windows, allow(dead_code))]
pub fn compose_unix(runner: &[(String, String)], places: &[(Source, Vec<(String, Option<String>)>)], login: &[(String, String)]) -> Composed {
    let mut c = Composed::from_runner(runner, false);
    for (source, vars) in places {
        for (k, v) in vars {
            match v {
                Some(v) => c.set(*source, k, v.clone()),
                None => c.env.remove(k),
            }
        }
    }
    for (k, v) in login {
        if runner.iter().rev().find(|(r, _)| r == k).map(|(_, rv)| rv) != Some(v) {
            c.set(Source::LoginShell, k, v.clone());
        }
    }
    c
}

/// The names in the domain's own `environment` block of `launchctl print gui/<uid>` (or
/// `user/<uid>`): what `launchctl setenv` set "on all future processes launched by launchd in the
/// caller's context" (launchctl(1)). Its output "is NOT API" (launchctl(1), `print`): names alone
/// are taken from it, each read again with `launchctl getenv`, which is; a block it no longer shows
/// so finds none.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_names(print: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in print.lines() {
        if !inside {
            // The domain's own block, one tab in; a service's would be deeper.
            inside = line == "\tenvironment = {";
            continue;
        }
        if line == "\t}" {
            break;
        }
        if let Some((name, _)) = line.strip_prefix("\t\t").and_then(|l| l.split_once(" => "))
            && super::valid_name(name)
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_owned());
        }
    }
    out
}

/// `launchctl getenv NAME`'s answer: its value and the newline it ends with; none when it prints
/// nothing (no such variable).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_value(out: &str) -> Option<String> {
    let v = out.strip_suffix('\n').unwrap_or(out);
    (!v.is_empty()).then(|| v.to_owned())
}

/// `systemctl --user show-environment`: "the environment block that is passed to all processes the
/// manager spawns" (`environment.d` and `systemctl --user set-environment` among it), one
/// `VARIABLE=value` a line, a value with whitespace or characters special to the shell written
/// `VARIABLE=$'value'`, dollar-single-quote escaped (systemctl(1), `show-environment`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_environment(text: &str) -> Vec<(String, Option<String>)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(k, _)| super::valid_name(k))
        .map(|(k, v)| (k.to_owned(), Some(v.strip_prefix("$'").and_then(|q| q.strip_suffix('\'')).map_or_else(|| v.to_owned(), unescape_dollar_single))))
        .collect()
}

/// A `$'…'` string's escapes, as bash reads them (bash(1), QUOTING: `\a \b \e \E \f \n \r \t \v \\
/// \' \" \?`, `\nnn` octal, `\xHH` hexadecimal); any other backslash is kept.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape_dollar_single(s: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0usize;
    while let Some(&c) = b.get(i) {
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let Some(&e) = b.get(i) else {
            out.push(b'\\');
            break;
        };
        i += 1;
        let simple = match e {
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            b'e' | b'E' => Some(0x1b),
            b'f' => Some(0x0c),
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'\\' | b'\'' | b'"' | b'?' => Some(e),
            _ => None,
        };
        if let Some(x) = simple {
            out.push(x);
            continue;
        }
        let (radix, max, start) = match e {
            b'0'..=b'7' => (8, 3, i.saturating_sub(1)),
            b'x' => (16, 2, i),
            _ => {
                out.extend([b'\\', e]);
                continue;
            }
        };
        let digits: Vec<u8> = b.get(start..).unwrap_or_default().iter().copied().take(max).take_while(|d| char::from(*d).is_digit(radix)).collect();
        match std::str::from_utf8(&digits).ok().and_then(|d| u32::from_str_radix(d, radix).ok()) {
            Some(n) => {
                out.push(u8::try_from(n & 0xff).unwrap_or(0));
                i = start + digits.len();
            }
            None => out.extend([b'\\', e]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `/etc/environment` as pam_env reads it (Linux-PAM `modules/pam_env/pam_env.c`,
/// `_parse_env_file`; pam_env(8), environment(5)): `KEY=VAL` a line, leading blanks skipped, a `#`
/// ending the line wherever it is, an `export ` before the key dropped, a key of letters, digits and
/// `_` alone; an opening quote dropped, and a quote that ends the line; a bare `KEY` takes it away
/// (`None`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pam_environment(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches([' ', '\t']);
        let line = line.split('#').next().unwrap_or_default();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            None => {
                if super::valid_name(line.trim_end()) {
                    out.push((line.trim_end().to_owned(), None));
                }
            }
            Some((k, v)) if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                let v = match v.strip_prefix(['"', '\'']) {
                    Some(inner) => inner.strip_suffix(['"', '\'']).unwrap_or(inner),
                    None => v,
                };
                out.push((k.to_owned(), Some(v.to_owned())));
            }
            Some(_) => {}
        }
    }
    out
}

/// What pam_env's configuration format sets (`/etc/security/pam_env.conf`, and `~/.pam_environment`
/// where pam_env reads it): `VARIABLE [DEFAULT=[value]] [OVERRIDE=[value]]` a line, `#` beginning a
/// comment, a `\` ending a line joining the next; a value quoted whole in `"…"`; `${VAR}` the value
/// pam_env set already (`set`, then what this file set above it), `@{HOME}` and `@{SHELL}` the
/// user's from `passwd` (`home`, `shell`), and `\$`, `\@`, `\\` themselves; OVERRIDE's when it's not
/// empty, else DEFAULT's (pam_env.conf(5); `_parse_config_file`, `_expand_arg`,
/// `_define_var`). One that's neither, or empty unquoted, sets nothing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pam_conf(text: &str, set: &[(String, String)], home: &str, shell: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let joined = text.replace("\\\n", "");
    for line in joined.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let mut words = line.split_whitespace();
        let Some(name) = words.next().filter(|n| super::valid_name(n)) else { continue };
        let rest: Vec<&str> = words.collect();
        let rest = rest.join(" ");
        let (default, overridden) = (conf_option(&rest, "DEFAULT="), conf_option(&rest, "OVERRIDE="));
        let known = |var: &str| -> String {
            out.iter().rev().find(|(k, _)| k == var).and_then(|(_, v)| v.clone()).or_else(|| set.iter().rev().find(|(k, _)| k == var).map(|(_, v)| v.clone())).unwrap_or_default()
        };
        let expand = |v: &str| conf_expand(v, &known, home, shell);
        let value = match (overridden.map(|(v, q)| (expand(&v), q)), default.map(|(v, q)| (expand(&v), q))) {
            (Some((v, quoted)), _) if !v.is_empty() || quoted => Some(v),
            (_, Some((v, quoted))) if !v.is_empty() || quoted => Some(v),
            _ => None,
        };
        if let Some(v) = value {
            out.push((name.to_owned(), Some(v)));
        }
    }
    out
}

/// The value of `KEY=` in `rest` (one quoted in `"…"` whole may hold blanks), and whether it was
/// quoted.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn conf_option(rest: &str, key: &str) -> Option<(String, bool)> {
    let at = rest.find(key).filter(|&at| at == 0 || rest.as_bytes().get(at - 1).is_some_and(u8::is_ascii_whitespace))?;
    let v = rest.get(at + key.len()..)?;
    if let Some(q) = v.strip_prefix('"') {
        return q.split_once('"').map(|(inner, _)| (inner.to_owned(), true));
    }
    Some((v.split_whitespace().next().unwrap_or_default().to_owned(), false))
}

/// `${VAR}`, `@{HOME}`, `@{SHELL}` and the escapes of pam_env's configuration format.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn conf_expand(v: &str, known: &dyn Fn(&str) -> String, home: &str, shell: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('$' | '@' | '\\')) => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '$' | '@' if chars.peek() == Some(&'{') => {
                chars.next();
                let name: String = chars.by_ref().take_while(|&ch| ch != '}').collect();
                match (c, name.as_str()) {
                    ('$', n) => out.push_str(&known(n)),
                    ('@', "HOME") => out.push_str(home),
                    ('@', "SHELL") => out.push_str(shell),
                    // Another PAM item (`@{PAM_RHOST}`) is the login's, which a runner has none of.
                    _ => {}
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// How pam_env is used, from what `/etc/pam.d`'s files say (each file's text): whether a stack
/// loads it at all (`pam_env.so`; Alpine's logins, with no PAM, never do), and whether one reads the
/// user's own file (`user_readenv=1`; off by default and deprecated since Linux-PAM 1.5.0,
/// pam_env(8)).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pam_env_use<'a>(files: impl IntoIterator<Item = &'a str>) -> (bool, bool) {
    let mut used = (false, false);
    for text in files {
        for line in text.lines().map(|l| l.split('#').next().unwrap_or_default()) {
            if line.split_whitespace().any(|w| w == "pam_env.so" || w.ends_with("/pam_env.so")) {
                used.0 = true;
                used.1 |= line.split_whitespace().any(|w| w == "user_readenv=1");
            }
        }
    }
    used
}

/// One value of a registry key of variables: `REG_SZ`, or `REG_EXPAND_SZ` (`expand`).
#[cfg_attr(not(windows), allow(dead_code))]
pub struct RegValue {
    pub name: String,
    pub data: String,
    pub expand: bool,
}

/// The environment a Windows user's new process gets now (what a sign-in or Explorer gives one,
/// `CreateEnvironmentBlock`), over the runner's own (`runner`: its profile's variables, `USERPROFILE`
/// and `APPDATA` among them, which no key here holds): the system's variables (`system`, `HKLM\…\Session
/// Manager\Environment`), then the user's (`user`, `HKCU\Environment`), the user's over the system's,
/// but `Path`, which is the system's with the user's after it. In each key `REG_SZ` values first, then
/// `REG_EXPAND_SZ` ones expanded with what's set so far; an empty one sets nothing. As Wine's
/// `CreateEnvironmentBlock` does it (dlls/userenv/userenv_main.c, `set_registry_variables`), and as
/// Microsoft says (KB 100843, "Environment variables in Windows NT": "The User path is appended to
/// the system path"). A runner's task keeps the environment it started with: a variable set since in
/// System Properties is in the registry alone.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn compose_windows(runner: &[(String, String)], system: &[RegValue], user: &[RegValue]) -> Composed {
    let mut c = Composed::from_runner(runner, true);
    let mut system_path = false;
    for (source, key) in [(Source::SystemRegistry, system), (Source::UserRegistry, user)] {
        for expand in [false, true] {
            for v in key.iter().filter(|v| v.expand == expand) {
                let data = if expand { expand_windows(&v.data, &c.env) } else { v.data.clone() };
                if data.is_empty() {
                    continue;
                }
                let is_path = v.name.eq_ignore_ascii_case("Path");
                let value = match (source, is_path, c.env.get(&v.name)) {
                    (Source::UserRegistry, true, Some(sys)) if system_path => format!("{sys};{data}"),
                    _ => data,
                };
                system_path |= is_path && source == Source::SystemRegistry;
                c.set(source, &v.name, value);
            }
        }
    }
    c
}

/// `%NAME%` in `text` replaced by `env`'s value of `NAME`, its case ignored; one `env` hasn't left
/// as it is ("If the name is not found, the %variableName% portion is left unexpanded", Microsoft
/// Learn, `ExpandEnvironmentStringsW`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn expand_windows(text: &str, env: &Env) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('%') {
        out.push_str(rest.get(..open).unwrap_or_default());
        let after = rest.get(open + 1..).unwrap_or_default();
        match after.find('%') {
            Some(close) => {
                let name = after.get(..close).unwrap_or_default();
                match env.get(name).filter(|_| !name.is_empty()) {
                    Some(v) => {
                        out.push_str(v);
                        rest = after.get(close + 1..).unwrap_or_default();
                    }
                    // Not a variable: its `%` kept, and its closing one may open the next.
                    None => {
                        out.push('%');
                        out.push_str(name);
                        rest = after.get(close..).unwrap_or_default();
                    }
                }
            }
            None => {
                out.push('%');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// As `launchctl print gui/501` shows its domain on macOS 26 (values here fake): the domain's
    /// own block alone, never a service's deeper one.
    #[test]
    fn launchds_names_are_its_domains_own_environment_blocks() {
        let print = "gui/501 = {\n\ttype = gui\n\tenvironment = {\n\t\tSSH_AUTH_SOCK => /private/tmp/x/Listeners\n\t\tANTHROPIC_API_KEY => fake\n\t\tbad name => x\n\t}\n\n\tservices = {\n\t\tenvironment = {\n\t\t\tNOT_THE_DOMAINS => y\n\t\t}\n\t}\n}\n";
        assert_eq!(launchd_names(print), ["SSH_AUTH_SOCK", "ANTHROPIC_API_KEY"]);
        assert!(launchd_names("gui/501 = {\n\ttype = gui\n}\n").is_empty());
        assert_eq!(launchd_value("v1\n").as_deref(), Some("v1"));
        assert_eq!(launchd_value("\n"), None);
    }

    #[test]
    fn systemds_environment_block_is_read_with_its_dollar_single_quotes() {
        let text = "HOME=/home/u\nLANG=C.UTF-8\nMULTI=$'a b\\nc\\'d\\\\e\\x41\\101'\nODD=$'\\q'\nbad-name=x\n";
        let got = systemd_environment(text);
        let v = |k: &str| got.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.clone());
        assert_eq!(v("HOME").as_deref(), Some("/home/u"));
        assert_eq!(v("MULTI").as_deref(), Some("a b\nc'd\\eAA"));
        assert_eq!(v("ODD").as_deref(), Some("\\q"));
        assert_eq!(got.len(), 4, "{:?}", got.iter().map(|(k, _)| k).collect::<Vec<_>>());
    }

    /// pam_env's `/etc/environment` (Ubuntu's PATH line among them).
    #[test]
    fn pams_etc_environment_is_read_as_pam_env_reads_it() {
        let text = "PATH=\"/usr/local/sbin:/usr/bin\"\n  # a comment\nexport EDITOR=vim # trailing\nQUOTED='x'\nGONE\nbad-key=1\n=nokey\n";
        let got = pam_environment(text);
        assert_eq!(got, vec![("PATH".into(), Some("/usr/local/sbin:/usr/bin".into())), ("EDITOR".into(), Some("vim ".into())), ("QUOTED".into(), Some("x".into())), ("GONE".into(), None)]);
    }

    #[test]
    fn pams_configuration_format_expands_as_pam_env_does() {
        let text = "# pam_env.conf\nEDITOR DEFAULT=vi OVERRIDE=${VISUAL}\nPAGER DEFAULT=less\nMYBIN DEFAULT=@{HOME}/bin:${PAGER}\nQUOTED DEFAULT=\"a b\"\nEMPTY DEFAULT=\"\"\nNOTHING\nLITERAL DEFAULT=\\${X}\nJOINED \\\n DEFAULT=yes\n";
        let got = pam_conf(text, &pairs(&[("VISUAL", "")]), "/home/u", "/bin/zsh");
        let v = |k: &str| got.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.clone());
        assert_eq!(v("EDITOR").as_deref(), Some("vi"), "OVERRIDE empty: DEFAULT's");
        assert_eq!(v("MYBIN").as_deref(), Some("/home/u/bin:less"));
        assert_eq!(v("QUOTED").as_deref(), Some("a b"));
        assert_eq!(v("EMPTY").as_deref(), Some(""));
        assert_eq!(v("NOTHING"), None);
        assert_eq!(v("LITERAL").as_deref(), Some("${X}"));
        assert_eq!(v("JOINED").as_deref(), Some("yes"));
        let got = pam_conf("EDITOR DEFAULT=vi OVERRIDE=${VISUAL}\n", &pairs(&[("VISUAL", "nano")]), "/h", "/s");
        assert_eq!(got, vec![("EDITOR".into(), Some("nano".into()))]);
    }

    #[test]
    fn pam_env_is_used_where_a_stack_loads_it() {
        let debian = "session required pam_env.so readenv=1\nsession required pam_env.so readenv=1 envfile=/etc/default/locale\n";
        assert_eq!(pam_env_use([debian]), (true, false));
        assert_eq!(pam_env_use(["auth required /lib/security/pam_env.so user_readenv=1\n"]), (true, true));
        assert_eq!(pam_env_use(["# session required pam_env.so user_readenv=1\nsession required pam_unix.so\n"]), (false, false));
        assert_eq!(pam_env_use(std::iter::empty::<&str>()), (false, false), "no /etc/pam.d (Alpine): none");
    }

    /// A terminal's environment: what launchd or the user manager has now over what the runner
    /// started with, under what the login shell's files set; a variable the shell only inherited
    /// from the runner doesn't hide a newer value.
    #[test]
    fn a_unix_users_environment_is_the_runners_under_its_places_under_its_shells_own() {
        let runner = pairs(&[("HOME", "/h"), ("STALE", "old"), ("PATH", "/usr/bin"), ("DROPPED", "1")]);
        let places = vec![(Source::Pam, vec![("DROPPED".into(), None), ("FROM_PAM".into(), Some("p".into()))]), (Source::Launchd, vec![("STALE".into(), Some("new".into()))])];
        let login = pairs(&[("HOME", "/h"), ("STALE", "old"), ("PATH", "/usr/bin:/opt/bin"), ("ANTHROPIC_API_KEY", "fake-key")]);
        let c = compose_unix(&runner, &places, &login);
        assert_eq!(c.env.get("STALE"), Some("new"));
        assert_eq!(c.env.get("PATH"), Some("/usr/bin:/opt/bin"));
        assert_eq!(c.env.get("ANTHROPIC_API_KEY"), Some("fake-key"));
        assert_eq!(c.env.get("FROM_PAM"), Some("p"));
        assert_eq!(c.env.get("DROPPED"), None);
        assert_eq!(c.sources.get("STALE").map(Vec::as_slice), Some(&[Source::Launchd, Source::Runner][..]));
        assert_eq!(c.sources.get("ANTHROPIC_API_KEY").map(Vec::as_slice), Some(&[Source::LoginShell][..]));
        assert_eq!(c.sources.get("HOME").map(Vec::as_slice), Some(&[Source::Runner][..]));
    }

    /// Windows: the system's then the user's, `REG_SZ` before `REG_EXPAND_SZ`, the user's `Path` after
    /// the system's, names one whatever their case, and the registry's values over what the runner
    /// started with.
    #[test]
    fn a_windows_users_environment_is_composed_as_windows_composes_it() {
        let reg = |name: &str, data: &str, expand: bool| RegValue { name: name.into(), data: data.into(), expand };
        let runner = pairs(&[("SystemRoot", "C:\\Windows"), ("USERPROFILE", "C:\\Users\\u"), ("PATH", "C:\\stale"), ("OLD_KEY", "before")]);
        let system = [reg("Path", "%SystemRoot%\\system32;%ProgramFiles%\\Git\\cmd", true), reg("ProgramFiles", "C:\\Program Files", false), reg("EMPTY", "", false)];
        let user = [reg("Path", "%USERPROFILE%\\.local\\bin", true), reg("old_key", "after", false), reg("TOOLS", "%NOPE%;%USERPROFILE%", true)];
        let c = compose_windows(&runner, &system, &user);
        assert_eq!(c.env.get("PATH"), Some("C:\\Windows\\system32;C:\\Program Files\\Git\\cmd;C:\\Users\\u\\.local\\bin"));
        assert_eq!(c.env.get("OLD_KEY"), Some("after"));
        assert_eq!(c.env.get("TOOLS"), Some("%NOPE%;C:\\Users\\u"));
        assert_eq!(c.env.get("EMPTY"), None);
        assert_eq!(c.sources.get("PATH").map(Vec::as_slice), Some(&[Source::UserRegistry, Source::SystemRegistry, Source::Runner][..]));
        // A user's Path with no system one is the user's alone.
        let c = compose_windows(&runner, &[], &[reg("Path", "C:\\u", false)]);
        assert_eq!(c.env.get("Path"), Some("C:\\u"));
    }

    #[test]
    fn windows_expansion_leaves_what_isnt_a_variable() {
        let mut env = Env::new(true);
        env.set("A", "x".into());
        for (text, want) in [("%a%", "x"), ("100%", "100%"), ("%%a%", "%x"), ("%B%%A%", "%B%x"), ("a%b", "a%b"), ("", "")] {
            assert_eq!(expand_windows(text, &env), want, "{text}");
        }
    }
}
