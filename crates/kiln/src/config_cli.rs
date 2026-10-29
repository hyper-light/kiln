//! `kiln config`: this machine's kiln config file from the command line: `path`, `show`, `get`,
//! `set` and `unset`, by dotted key (`updates.accept`). A change keeps the file's comments and
//! layout, and is checked against what kiln reads (unknown keys and wrong types are refused)
//! before it is written; the file keeps its permissions (it may hold secrets).

use std::path::Path;

use anyhow::{Context as _, Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    cmd: ConfigCmd,
}

#[derive(clap::Subcommand)]
enum ConfigCmd {
    /// Where the config file is.
    Path,
    /// The config as kiln reads it (the file over kiln's defaults), or one section of it. Secrets
    /// show as <set>.
    Show { section: Option<String> },
    /// One value, by its dotted key: `kiln config get updates.accept`.
    Get { key: String },
    /// Set a value: `kiln config set updates.accept false`. The value is TOML (true, 30, "text",
    /// ["a", "b"]); a bare word is a string.
    Set { key: String, value: String },
    /// Take a value out of the file: kiln's default applies again.
    Unset { key: String },
}

pub fn run(args: ConfigArgs, path: &Path) -> Result<()> {
    match args.cmd {
        ConfigCmd::Path => {
            println!("{}", path.display());
            Ok(())
        }
        ConfigCmd::Show { section } => {
            let v = effective(path)?;
            let v = match &section {
                Some(s) => at(&v, s).with_context(|| format!("the config has no section {s}"))?.clone(),
                None => v,
            };
            let mut v = v;
            redact(&mut v);
            match &v {
                toml::Value::Table(_) => print!("{}", toml::to_string_pretty(&v)?),
                other => println!("{other}"),
            }
            Ok(())
        }
        ConfigCmd::Get { key } => {
            let v = effective(path)?;
            match at(&v, &key) {
                Some(toml::Value::String(s)) => println!("{s}"),
                Some(toml::Value::Table(_)) => print!("{}", toml::to_string_pretty(at(&v, &key).unwrap_or(&v))?),
                Some(other) => println!("{other}"),
                None => bail!("the config has no key {key}"),
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let item = value.parse::<toml_edit::Value>().unwrap_or_else(|_| toml_edit::Value::from(value.as_str()));
            edit(path, &key, Some(item))?;
            println!("{key} = {} in {}", show_set(&key, &value), path.display());
            println!("{}", when(&key));
            Ok(())
        }
        ConfigCmd::Unset { key } => {
            edit(path, &key, None)?;
            println!("{key} taken out of {}: kiln's default applies", path.display());
            println!("{}", when(&key));
            Ok(())
        }
    }
}

/// The config kiln reads: the file over its defaults, as a TOML value.
fn effective(path: &Path) -> Result<toml::Value> {
    let cfg = Config::load(path)?;
    toml::Value::try_from(&cfg).context("the config doesn't serialize")
}

fn at<'a>(v: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    key.split('.').try_fold(v, |v, k| v.get(k))
}

/// What says secret by its name.
fn secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["token", "secret", "password", "private_key", "api_key", "client_key"].iter().any(|s| n.contains(s))
}

fn redact(v: &mut toml::Value) {
    match v {
        toml::Value::Table(t) => {
            for (k, v) in t.iter_mut() {
                if secret(k) && matches!(v, toml::Value::String(s) if !s.is_empty()) {
                    *v = toml::Value::String("<set>".to_owned());
                } else {
                    redact(v);
                }
            }
        }
        toml::Value::Array(a) => a.iter_mut().for_each(redact),
        _ => {}
    }
}

fn show_set(key: &str, value: &str) -> String {
    if key.rsplit('.').next().is_some_and(secret) { "<set>".to_owned() } else { value.to_owned() }
}

/// When a change takes effect.
fn when(key: &str) -> &'static str {
    match key {
        "updates.accept" => "This machine's runner reads it within a minute and tells its server; nothing needs restarting.",
        "agent.max_agent_sessions" => {
            "This machine's runner reads it within seconds, nothing restarting (a server reads its own as it starts); what's set for the machine on the server's Runners page wins over it."
        }
        _ => "kiln reads its config when it starts: restart it (its service) for this to take effect.",
    }
}

/// Set (or, with `None`, take out) `key` in the file at `path`, keeping its comments and layout;
/// refused unless kiln can read the result.
fn edit(path: &Path, key: &str, value: Option<toml_edit::Value>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && value.is_some() => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("parse {}", path.display()))?;
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, tables)) = parts.split_last() else { bail!("no key") };
    if parts.iter().any(|p| p.is_empty()) {
        bail!("{key:?} isn't a dotted key (like updates.accept)");
    }
    let mut table = doc.as_table_mut();
    for t in tables {
        if value.is_none() && !table.contains_key(t) {
            bail!("the file has no {key}");
        }
        let entry = table.entry(t).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table = entry.as_table_mut().with_context(|| format!("{t} in {key} isn't a section"))?;
    }
    match value {
        Some(v) => {
            table[last] = toml_edit::value(v);
        }
        None => {
            if table.remove(last).is_none() {
                bail!("the file has no {key}");
            }
        }
    }
    let out = doc.to_string();
    // What kiln reads: unknown keys and wrong types are refused here, not when it next starts.
    toml::from_str::<Config>(&out).with_context(|| format!("kiln wouldn't read {key} so"))?;
    write(path, out.as_bytes())
}

/// Write `bytes` over `path` at once (a temporary file renamed into place), keeping its
/// permissions (new: only its owner can read it).
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777)
    };
    #[cfg(not(unix))]
    let mode = 0o600;
    crate::util::write_secret(&tmp, bytes, mode).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_keeps_the_file_and_is_checked() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("kiln.toml");
        std::fs::write(&path, "# the server\n[server]\nhttp_addr = \"0.0.0.0:7070\" # kept\n")?;
        edit(&path, "updates.accept", Some("false".parse()?))?;
        let text = std::fs::read_to_string(&path)?;
        assert!(text.contains("# the server") && text.contains("# kept"), "comments kept: {text}");
        assert!(!Config::load(&path)?.updates.accept);
        // Unknown keys and wrong types are refused, and the file is left as it was.
        assert!(edit(&path, "updates.nonsense", Some("1".parse()?)).is_err());
        assert!(edit(&path, "updates.accept", Some("\"maybe\"".parse()?)).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, text);
        edit(&path, "updates.accept", None)?;
        assert!(Config::load(&path)?.updates.accept);
        assert!(edit(&path, "updates.accept", None).is_err(), "nothing to take out");
        Ok(())
    }

    #[test]
    fn secrets_show_as_set() {
        let mut v: toml::Value = toml::from_str("[a]\napi_token = \"x\"\nname = \"n\"\n").unwrap_or(toml::Value::Integer(0));
        redact(&mut v);
        assert_eq!(at(&v, "a.api_token").and_then(toml::Value::as_str), Some("<set>"));
        assert_eq!(at(&v, "a.name").and_then(toml::Value::as_str), Some("n"));
    }
}
