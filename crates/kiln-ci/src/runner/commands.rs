//! Workflow commands (docs/ci.md W06-R06), GitHub-exact: one per line of a step's output,
//! `::name key=value,key=value::data` (and the older `##[name key=value;]data`). Data unescapes
//! `%25`, `%0D` and `%0A`; property values also `%3A` and `%2C`. `::stop-commands::<token>` stops
//! processing until a line `::<token>::`.

/// A command a line carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    AddMask(String),
    Group(String),
    EndGroup,
    Debug(String),
    /// `notice`, `warning` or `error`, with GitHub's annotation properties.
    Annotation(Annotation),
    StopCommands(String),
    Echo(bool),
    AddMatcher(String),
    /// By `owner`, or by the owners of the matcher file the data names.
    RemoveMatcher { owner: String, file: String },
    /// Deprecated: honoured, with a warning.
    SetOutput { name: String, value: String },
    SaveState { name: String, value: String },
    /// Refused unless `ACTIONS_ALLOW_UNSECURE_COMMANDS=true`.
    SetEnv { name: String, value: String },
    AddPath(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Annotation {
    /// `notice`, `warning`, `error`.
    pub level: String,
    pub message: String,
    pub title: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub col: Option<u32>,
    pub end_col: Option<u32>,
}

/// GitHub's data escapes, undone.
pub fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%25", "%")
}

/// GitHub's property escapes, undone (data's plus `:` and `,`).
pub fn unescape_property(s: &str) -> String {
    s.replace("%0D", "\r").replace("%0A", "\n").replace("%3A", ":").replace("%2C", ",").replace("%25", "%")
}

/// The escapes the toolkit writes (for messages kiln itself emits).
pub fn escape_data(s: &str) -> String {
    s.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// A line split into command name, properties and data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub name: String,
    pub props: Vec<(String, String)>,
    pub data: String,
}

/// Reads `::name props::data` (the `::` form) or `##[name props]data` (the older form, `;`
/// between properties). `None` when the line is no command.
pub fn parse(line: &str) -> Option<Parsed> {
    let line = line.trim_end_matches(['\r', '\n']);
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("::") {
        let end = rest.find("::")?;
        let head = rest.get(..end)?;
        let data = rest.get(end + 2..)?;
        let (name, props) = match head.split_once(' ') {
            Some((n, p)) => (n, p),
            None => (head, ""),
        };
        if name.is_empty() {
            return None;
        }
        let props = props
            .split(',')
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_owned(), unescape_property(v))))
            .collect();
        return Some(Parsed { name: name.to_owned(), props, data: unescape_data(data) });
    }
    if let Some(rest) = trimmed.strip_prefix("##[") {
        let end = rest.find(']')?;
        let head = rest.get(..end)?;
        let data = rest.get(end + 1..)?;
        let (name, props) = match head.split_once(' ') {
            Some((n, p)) => (n, p),
            None => (head, ""),
        };
        let props = props.split(';').filter(|p| !p.trim().is_empty()).filter_map(|p| p.split_once('=').map(|(k, v)| (k.trim().to_owned(), unescape_property(v)))).collect();
        return Some(Parsed { name: name.to_owned(), props, data: unescape_data(data) });
    }
    None
}

fn prop<'a>(p: &'a Parsed, k: &str) -> Option<&'a str> {
    p.props.iter().find(|(x, _)| x.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
}

fn num(p: &Parsed, k: &str) -> Option<u32> {
    prop(p, k).and_then(|v| v.trim().parse().ok())
}

/// What one line does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// Ordinary output.
    Text,
    /// A command to carry out.
    Command(Command),
    /// A command refused, with the message to log (an error annotation, as GitHub does).
    Refused(String),
    /// A command name the runner doesn't know: shown as text.
    Unknown,
}

/// The per-step command processor: `stop-commands` and `echo` state.
#[derive(Debug, Clone, Default)]
pub struct Processor {
    stopped: Option<String>,
    pub echo: bool,
    /// `ACTIONS_ALLOW_UNSECURE_COMMANDS=true`.
    pub allow_unsecure: bool,
}

impl Processor {
    pub fn new(allow_unsecure: bool) -> Self {
        Processor { stopped: None, echo: false, allow_unsecure }
    }

    /// Whether processing is stopped (`::stop-commands::`).
    pub fn stopped(&self) -> bool {
        self.stopped.is_some()
    }

    pub fn line(&mut self, line: &str) -> Line {
        let Some(p) = parse(line) else { return Line::Text };
        if let Some(token) = &self.stopped {
            // Resuming is not a command of its own; everything else stays text until then.
            if p.name == *token {
                self.stopped = None;
            }
            return Line::Text;
        }
        let cmd = match p.name.as_str() {
            "add-mask" => Command::AddMask(p.data.trim().to_owned()),
            "group" => Command::Group(p.data.clone()),
            "endgroup" => Command::EndGroup,
            "debug" => Command::Debug(p.data.clone()),
            "notice" | "warning" | "error" => Command::Annotation(Annotation {
                level: p.name.clone(),
                message: p.data.clone(),
                title: prop(&p, "title").map(str::to_owned),
                file: prop(&p, "file").map(str::to_owned),
                line: num(&p, "line"),
                end_line: num(&p, "endLine"),
                col: num(&p, "col"),
                end_col: num(&p, "endColumn"),
            }),
            "stop-commands" => {
                if p.data.is_empty() {
                    return Line::Refused("Invalid stop-commands: a token is required".into());
                }
                self.stopped = Some(p.data.clone());
                Command::StopCommands(p.data.clone())
            }
            "echo" => match p.data.trim() {
                "on" => {
                    self.echo = true;
                    Command::Echo(true)
                }
                "off" => {
                    self.echo = false;
                    Command::Echo(false)
                }
                other => return Line::Refused(format!("Invalid echo command value '{other}'. Expected 'on' or 'off'.")),
            },
            "add-matcher" => Command::AddMatcher(p.data.clone()),
            "remove-matcher" => Command::RemoveMatcher { owner: prop(&p, "owner").unwrap_or_default().to_owned(), file: p.data.trim().to_owned() },
            "set-output" => Command::SetOutput { name: prop(&p, "name").unwrap_or_default().to_owned(), value: p.data.clone() },
            "save-state" => Command::SaveState { name: prop(&p, "name").unwrap_or_default().to_owned(), value: p.data.clone() },
            "set-env" | "add-path" if !self.allow_unsecure => {
                return Line::Refused(format!(
                    "The `{}` command is disabled. Please upgrade to using Environment Files or opt into unsecure command execution by setting the `ACTIONS_ALLOW_UNSECURE_COMMANDS` environment variable to `true`. For more information see: https://github.blog/changelog/2020-10-01-github-actions-deprecating-set-env-and-add-path-commands/",
                    p.name
                ));
            }
            "set-env" => Command::SetEnv { name: prop(&p, "name").unwrap_or_default().to_owned(), value: p.data.clone() },
            "add-path" => Command::AddPath(p.data.clone()),
            _ => return Line::Unknown,
        };
        Line::Command(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W06-AC02: each command, escapes and properties.
    #[test]
    fn w06_ac02_commands() {
        let mut p = Processor::new(false);
        assert_eq!(p.line("plain text"), Line::Text);
        assert_eq!(p.line("::add-mask::  s3cret "), Line::Command(Command::AddMask("s3cret".into())));
        assert_eq!(p.line("::group::Build %25 things"), Line::Command(Command::Group("Build % things".into())));
        assert_eq!(p.line("::endgroup::"), Line::Command(Command::EndGroup));
        assert_eq!(p.line("::debug::a%0Ab"), Line::Command(Command::Debug("a\nb".into())));
        let ann = p.line("::error file=src/a.rs,line=3,col=5,endLine=4,endColumn=9,title=Bad%3A thing%2C really::bad%0D%0Anews");
        assert_eq!(
            ann,
            Line::Command(Command::Annotation(Annotation {
                level: "error".into(),
                message: "bad\r\nnews".into(),
                title: Some("Bad: thing, really".into()),
                file: Some("src/a.rs".into()),
                line: Some(3),
                end_line: Some(4),
                col: Some(5),
                end_col: Some(9),
            }))
        );
        assert!(matches!(p.line("::warning::w"), Line::Command(Command::Annotation(a)) if a.level == "warning"));
        assert!(matches!(p.line("::notice title=N::n"), Line::Command(Command::Annotation(a)) if a.title.as_deref() == Some("N")));
        assert_eq!(p.line("::set-output name=x::1"), Line::Command(Command::SetOutput { name: "x".into(), value: "1".into() }));
        assert_eq!(p.line("::save-state name=s::v"), Line::Command(Command::SaveState { name: "s".into(), value: "v".into() }));
        assert!(matches!(p.line("::set-env name=A::b"), Line::Refused(m) if m.contains("ACTIONS_ALLOW_UNSECURE_COMMANDS")));
        assert!(matches!(p.line("::add-path::/x"), Line::Refused(_)));
        assert_eq!(Processor::new(true).line("::set-env name=A::b"), Line::Command(Command::SetEnv { name: "A".into(), value: "b".into() }));
        assert_eq!(p.line("::add-matcher::.github/tsc.json"), Line::Command(Command::AddMatcher(".github/tsc.json".into())));
        assert_eq!(p.line("::remove-matcher owner=tsc::"), Line::Command(Command::RemoveMatcher { owner: "tsc".into(), file: String::new() }));
        assert_eq!(p.line("::remove-matcher::.github/tsc.json"), Line::Command(Command::RemoveMatcher { owner: String::new(), file: ".github/tsc.json".into() }));
        assert_eq!(p.line("::bogus::x"), Line::Unknown);
        // The older form.
        assert_eq!(p.line("##[set-output name=y;]2"), Line::Command(Command::SetOutput { name: "y".into(), value: "2".into() }));
        assert_eq!(p.line("##[group]G"), Line::Command(Command::Group("G".into())));
        // Leading whitespace is allowed, as the runner allows it.
        assert_eq!(p.line("   ::endgroup::"), Line::Command(Command::EndGroup));
    }

    #[test]
    fn w06_ac02_stop_commands_and_echo() {
        let mut p = Processor::new(false);
        assert_eq!(p.line("::stop-commands::pause-123"), Line::Command(Command::StopCommands("pause-123".into())));
        assert!(p.stopped());
        assert_eq!(p.line("::error::not processed"), Line::Text);
        assert_eq!(p.line("::set-output name=x::nope"), Line::Text);
        assert_eq!(p.line("::pause-123::"), Line::Text, "the resume line itself does nothing");
        assert!(!p.stopped());
        assert!(matches!(p.line("::error::processed"), Line::Command(_)));
        assert_eq!(p.line("::echo::on"), Line::Command(Command::Echo(true)));
        assert!(p.echo);
        assert!(matches!(p.line("::echo::loud"), Line::Refused(_)));
        assert!(matches!(p.line("::stop-commands::"), Line::Refused(_)));
    }

    #[test]
    fn escapes_round_trip() {
        for s in ["a%b", "line1\nline2", "cr\r\nlf", "%0A literal"] {
            assert_eq!(unescape_data(&escape_data(s)), s);
        }
    }
}
