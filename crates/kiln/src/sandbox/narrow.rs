//! What an isolated run reaches past its microVM, of what its image grants (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 17 part 6; docs/isolation.md). Shards' D236 lets a run drop
//! its image's destinations, never add one (`Narrowing.destinations`). Each of kiln's built-ins names
//! every host its harness reaches for every provider or sign-in it serves (step 18 part 4: ROOT's call
//! of 2026-11-13, on the person's bar of zero breaks), and a run keeps only its own session's
//! (`agentfiles::builtin::destinations::BY_USE`): what its harness selects them by, read as the harness
//! reads it, its run's environment first, then the model its session names, then its configuration in
//! its home (the person's own volume, `harness::isolated::home`).
//!
//! Where kiln can't tell what a run uses, it keeps its image whole and says why ([`Whole`]), on the
//! session's `runs.reach` and as a notice: never a silent widening (ROOT, 2026-11-13). A harness's
//! configuration that its harness would refuse (a file it can't parse) is kiln's to say, not to guess
//! past.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agentfiles::builtin::destinations::BY_USE;

/// What a run reaches past its microVM, of what its image grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentReach"))]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum Reach {
    /// Only these of its image's destinations (`host:port`), those of what its session uses (`uses`,
    /// by `BY_USE`'s ids), the rest dropped.
    Narrowed { uses: Vec<String>, hosts: Vec<String> },
    /// Every destination its image grants, and why kiln didn't narrow them.
    Whole { why: Whole },
}

/// Why a run keeps every destination its image grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentWhole"))]
#[serde(rename_all = "snake_case")]
pub enum Whole {
    /// Its image serves one provider or sign-in: none of its destinations is another use's to drop.
    OneUse,
    /// A person's own Agentfile: its grants are as its person wrote them.
    PersonsOwn,
    /// kiln keeps no home for it on this machine to read what it uses from: the session acts for no
    /// person, or this machine keeps no homes.
    NoHome,
    /// What it uses couldn't be read: the file, by its path in the microVM, and why.
    Unread { file: String, error: String },
    /// What it uses is no use its image names hosts for: `value`, as `what` names it.
    Unnamed { what: String, value: String },
    /// Nothing it reads says what it uses: where it looks.
    Untold { what: String },
}

impl Whole {
    /// Whether it keeps more than its session uses because kiln couldn't tell what that is: the
    /// widening the session says, as against an image whose destinations are all its run's.
    pub fn is_fallback(&self) -> bool {
        !matches!(self, Whole::OneUse | Whole::PersonsOwn)
    }
}

impl std::fmt::Display for Whole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Whole::OneUse => f.write_str("its image serves one provider or sign-in"),
            Whole::PersonsOwn => f.write_str("its Agentfile is its person's own, its grants as they wrote them"),
            Whole::NoHome => f.write_str("kiln keeps no home for it on this machine to read its provider or sign-in from"),
            Whole::Unread { file, error } => write!(f, "{file} couldn't be read: {error}"),
            Whole::Unnamed { what, value } => write!(f, "{what} is {value:?}, which its image names no hosts for"),
            Whole::Untold { what } => write!(f, "nothing says its provider or sign-in ({what})"),
        }
    }
}

/// What a run reads to tell what it uses.
pub struct Reads<'a> {
    /// Its harness's home in the microVM (`HOME`).
    pub home: &'a str,
    /// Its project's folder, at its own path in the microVM (Shards D235).
    pub project: Option<&'a str>,
    /// Its run's environment, as the microVM has it.
    pub env: &'a [(String, String)],
    /// The model its session names, as its start asked for it.
    pub model: Option<&'a str>,
    /// A file in the microVM, by its path there, as this machine reads it: none where it isn't.
    pub read: &'a dyn Fn(&str) -> Result<Option<String>, String>,
}

impl Reads<'_> {
    fn var(&self, key: &str) -> Option<&str> {
        self.env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
    }

    /// The configuration directory its harness reads, as the XDG Base Directory Specification 0.8 has it.
    fn config_dir(&self) -> String {
        self.var("XDG_CONFIG_HOME").map_or_else(|| format!("{}/.config", self.home), str::to_owned)
    }

    fn text(&self, file: &str) -> Result<Option<String>, Whole> {
        (self.read)(file).map_err(|error| Whole::Unread { file: file.to_owned(), error })
    }

    /// A JSON file (with JSONC's comments and trailing commas, as opencode's and Antigravity's own
    /// readers take them), none where it isn't.
    fn json(&self, file: &str) -> Result<Option<Value>, Whole> {
        let Some(text) = self.text(file)? else {
            return Ok(None);
        };
        serde_json::from_str(&kiln_cd::jsonc::strip(&text)).map(Some).map_err(|e| Whole::Unread { file: file.to_owned(), error: e.to_string() })
    }

    fn yaml(&self, file: &str) -> Result<Option<serde_yaml::Value>, Whole> {
        let Some(text) = self.text(file)? else {
            return Ok(None);
        };
        serde_yaml::from_str(&text).map(Some).map_err(|e| Whole::Unread { file: file.to_owned(), error: e.to_string() })
    }
}

/// What a run of `harness` reaches, given what it reads: its image's destinations narrowed to those of
/// what its session uses, else whole and why.
pub fn reach(harness: &str, r: &Reads<'_>) -> Reach {
    let Some(table) = BY_USE.iter().find(|t| t.harness == harness) else {
        return Reach::Whole { why: Whole::OneUse };
    };
    let uses = match harness {
        "antigravity" => antigravity(r),
        "goose" => goose(r),
        "opencode" => opencode(r),
        "pi" => pi(r),
        other => Err(Whole::Untold { what: format!("kiln reads no provider or sign-in of {other}'s") }),
    };
    let uses = match uses {
        Ok(uses) => uses,
        Err(why) => return Reach::Whole { why },
    };
    let mut hosts: Vec<String> = table.always.iter().map(|h| (*h).to_owned()).collect();
    for (what, value) in &uses {
        match table.by.iter().find(|(id, _)| id == value) {
            Some((_, of)) => hosts.extend(of.iter().map(|h| (*h).to_owned())),
            None => return Reach::Whole { why: Whole::Unnamed { what: what.clone(), value: value.clone() } },
        }
    }
    hosts.sort();
    hosts.dedup();
    Reach::Narrowed { uses: uses.into_iter().map(|(_, v)| v).collect(), hosts }
}

/// Each use a run reads, as `(where it read it, BY_USE's id)`.
type Uses = Result<Vec<(String, String)>, Whole>;

/// Antigravity's sign-in, as its docs order them (`install.md`'s "Authenticate", `enterprise.md`'s "Sign
/// in and license selection"; docs/research/s26-agy-agentfile, "The sign-ins and their hosts"): a Gemini
/// API key where its settings name the `gemini` provider (the key the session's own, never kiln's to
/// see); ADC where `AGY_ADC_AUTH` or `enableAdc` asks for it, at `GOOGLE_CLOUD_LOCATION` (`global` by
/// default); Gemini Enterprise where its settings keep a license's `gcp` location; else a personal Google
/// account. Workforce Identity Federation's token exchange (`sts`) goes with an enterprise sign-in kiln
/// can't tell from SSO by its settings, and with ADC but where its credentials file says it's no
/// external account's.
fn antigravity(r: &Reads<'_>) -> Uses {
    let settings_file = format!("{}/.gemini/antigravity-cli/settings.json", r.home);
    let settings = r.json(&settings_file)?;
    if settings.as_ref().and_then(|s| s.get("modelProvider")).and_then(Value::as_str) == Some("gemini") {
        return Ok(vec![(format!("{settings_file}'s modelProvider"), "gemini-api-key".to_owned())]);
    }
    let config_file = format!("{}/.gemini/config/config.json", r.home);
    let enable_adc = r.json(&config_file)?.as_ref().and_then(|c| c.get("enableAdc")).and_then(Value::as_bool) == Some(true);
    if r.var("AGY_ADC_AUTH") == Some("true") || enable_adc {
        let location = r.var("GOOGLE_CLOUD_LOCATION").unwrap_or("global");
        let mut uses = vec![("GOOGLE_CLOUD_LOCATION".to_owned(), enterprise("GOOGLE_CLOUD_LOCATION", location)?)];
        let adc_file = r.var("GOOGLE_APPLICATION_CREDENTIALS").map_or_else(|| format!("{}/gcloud/application_default_credentials.json", r.config_dir()), str::to_owned);
        let kind = r.json(&adc_file)?.and_then(|c| c.get("type").and_then(Value::as_str).map(str::to_owned));
        if kind.as_deref() != Some("authorized_user") {
            uses.push((format!("{adc_file}'s type"), "workforce-identity-federation".to_owned()));
        }
        return Ok(uses);
    }
    if let Some(gcp) = settings.as_ref().and_then(|s| s.get("gcp")) {
        let location = gcp.get("location").and_then(Value::as_str).unwrap_or("global");
        let what = format!("{settings_file}'s gcp.location");
        return Ok(vec![
            (what.clone(), enterprise(&what, location)?),
            (format!("{settings_file}'s gcp"), "workforce-identity-federation".to_owned()),
        ]);
    }
    Ok(vec![(format!("{settings_file} (no API key, ADC or license)"), "google-account".to_owned())])
}

/// Antigravity's enterprise sign-in at `location`, as `BY_USE` names it (`enterprise-global`, `-us`,
/// `-eu`): a location its image names no hosts for is said as it was read, by `what`.
fn enterprise(what: &str, location: &str) -> Result<String, Whole> {
    let id = format!("enterprise-{location}");
    let named = BY_USE.iter().any(|t| t.harness == "antigravity" && t.by.iter().any(|(i, _)| *i == id));
    if named { Ok(id) } else { Err(Whole::Unnamed { what: what.to_owned(), value: location.to_owned() }) }
}

/// Goose's provider (`GOOSE_PROVIDER`, its environment's over its `config.yaml`'s), and TypeSafe's auto
/// effort where it's enabled (`GOOSE_AUTO_EFFORT_ENABLED`; its key the session's own). Its `config.yaml`
/// is under `config/` in `GOOSE_PATH_ROOT` where its environment roots Goose's files there, as kiln's
/// image does (its harness.json's `run.env`), else under `goose/` in its configuration directory, as
/// `goose info -v` says (docs/research/s23-goose-agentfile/evidence/e_runtime_linux_x86_64.txt).
fn goose(r: &Reads<'_>) -> Uses {
    let file = match r.var("GOOSE_PATH_ROOT") {
        Some(root) => format!("{}/config/config.yaml", root.trim_end_matches('/')),
        None => format!("{}/goose/config.yaml", r.config_dir()),
    };
    let config = r.yaml(&file)?;
    let said = |key: &str| r.var(key).map(str::to_owned).or_else(|| config.as_ref().and_then(|c| c.get(key)).and_then(yaml_text));
    let provider = said("GOOSE_PROVIDER").ok_or_else(|| Whole::Untold { what: format!("GOOSE_PROVIDER, in its environment or {file}") })?;
    let mut uses = vec![("GOOSE_PROVIDER".to_owned(), provider)];
    if said("GOOSE_AUTO_EFFORT_ENABLED").is_some_and(|v| v == "true") {
        uses.push(("GOOSE_AUTO_EFFORT_ENABLED".to_owned(), "auto-effort".to_owned()));
    }
    Ok(uses)
}

/// A YAML scalar as its text (Goose's `config.yaml` keeps `true` and names alike).
fn yaml_text(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// opencode's provider: its `model`'s (`provider/model`), the session's own first, then its project's
/// `opencode.json[c]`, then its global one, as opencode merges them, a project's over its own.
fn opencode(r: &Reads<'_>) -> Uses {
    let mut files: Vec<String> = Vec::new();
    if let Some(project) = r.project {
        files.extend(["opencode.json", "opencode.jsonc"].map(|f| format!("{}/{f}", project.trim_end_matches('/'))));
    }
    files.extend(["opencode.json", "opencode.jsonc"].map(|f| format!("{}/opencode/{f}", r.config_dir())));
    let mut model = r.model.map(|m| ("its session's model".to_owned(), m.to_owned()));
    for file in &files {
        if model.is_some() {
            break;
        }
        model = r.json(file)?.and_then(|c| c.get("model").and_then(Value::as_str).map(|m| (format!("{file}'s model"), m.to_owned())));
    }
    let (from, model) = model.ok_or_else(|| Whole::Untold { what: format!("a model, in its session or {}", files.join(", ")) })?;
    let provider = model.split_once('/').map(|(p, _)| p.to_owned()).filter(|p| !p.is_empty()).ok_or_else(|| Whole::Unnamed { what: from.clone(), value: model.clone() })?;
    Ok(vec![(from, provider)])
}

/// pi's provider: its settings' `defaultProvider`, in `settings.json` in its agent directory:
/// `PI_CODING_AGENT_DIR` where its environment names one, as kiln's image does (its harness.json's
/// `run.env`), else `~/.pi/agent`.
fn pi(r: &Reads<'_>) -> Uses {
    let dir = r.var("PI_CODING_AGENT_DIR").map_or_else(|| format!("{}/.pi/agent", r.home), |d| d.trim_end_matches('/').to_owned());
    let file = format!("{dir}/settings.json");
    let provider = r.json(&file)?.and_then(|s| s.get("defaultProvider").and_then(Value::as_str).map(str::to_owned));
    let provider = provider.ok_or_else(|| Whole::Untold { what: format!("defaultProvider, in {file}") })?;
    Ok(vec![(format!("{file}'s defaultProvider"), provider)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a run of `harness` reaches, its home `/home/h`, its project `/p`, given `env`, `model` and
    /// `files` (by path in the microVM).
    fn of(harness: &str, env: &[(&str, &str)], model: Option<&str>, files: &[(&str, &str)]) -> Reach {
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let read = |path: &str| -> Result<Option<String>, String> { Ok(files.iter().find(|(p, _)| *p == path).map(|(_, t)| (*t).to_owned())) };
        reach(harness, &Reads { home: "/home/h", project: Some("/p"), env: &env, model, read: &read })
    }

    fn row(harness: &str, id: &str) -> Vec<String> {
        let table = BY_USE.iter().find(|t| t.harness == harness);
        let mut hosts: Vec<String> = table.map(|t| t.always.iter().map(|h| (*h).to_owned()).collect()).unwrap_or_default();
        hosts.extend(table.and_then(|t| t.by.iter().find(|(i, _)| *i == id)).map(|(_, h)| h.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>()).unwrap_or_default());
        hosts
    }

    fn narrowed(r: &Reach) -> (Vec<String>, Vec<String>) {
        match r {
            Reach::Narrowed { uses, hosts } => (uses.clone(), hosts.clone()),
            Reach::Whole { why } => (Vec::new(), vec![format!("whole: {why}")]),
        }
    }

    /// Step 17 part 6: each of Antigravity's four sign-ins keeps its own hosts alone, as its docs select
    /// it: a Gemini API key; ADC at its location (with WIF's token exchange, but for an authorized
    /// user's credentials); a license's location; else a personal Google account.
    #[test]
    fn s17_an_antigravity_run_keeps_its_own_sign_ins_hosts() {
        let settings = "/home/h/.gemini/antigravity-cli/settings.json";
        let key = of("antigravity", &[], None, &[(settings, "{\"modelProvider\": \"gemini\"}")]);
        assert_eq!(narrowed(&key), (vec!["gemini-api-key".to_owned()], row("antigravity", "gemini-api-key")));
        let personal = of("antigravity", &[], None, &[]);
        let mut google = row("antigravity", "google-account");
        google.sort();
        assert_eq!(narrowed(&personal), (vec!["google-account".to_owned()], google));
        let adc = of("antigravity", &[("AGY_ADC_AUTH", "true"), ("GOOGLE_CLOUD_LOCATION", "us")], None, &[("/home/h/.config/gcloud/application_default_credentials.json", "{\"type\": \"authorized_user\"}")]);
        assert_eq!(narrowed(&adc).0, ["enterprise-us"], "an authorized user's ADC refreshes at oauth2 alone");
        let wif = of("antigravity", &[], None, &[("/home/h/.gemini/config/config.json", "{\"enableAdc\": true, // its own\n}"), ("/home/h/.config/gcloud/application_default_credentials.json", "{\"type\": \"external_account\"}")]);
        assert_eq!(narrowed(&wif).0, ["enterprise-global", "workforce-identity-federation"]);
        let license = of("antigravity", &[], None, &[(settings, "{\"gcp\": {\"project\": \"p\", \"location\": \"eu\"}}")]);
        assert_eq!(narrowed(&license).0, ["enterprise-eu", "workforce-identity-federation"]);
        let mut eu: Vec<String> = row("antigravity", "enterprise-eu").into_iter().chain(row("antigravity", "workforce-identity-federation")).collect();
        eu.sort();
        eu.dedup();
        assert_eq!(narrowed(&license).1, eu, "its location's hosts and WIF's, none of the others'");
        // A location its image names no hosts for keeps the image whole, saying so.
        let elsewhere = of("antigravity", &[("AGY_ADC_AUTH", "true"), ("GOOGLE_CLOUD_LOCATION", "asia-south1")], None, &[]);
        assert!(matches!(&elsewhere, Reach::Whole { why: Whole::Unnamed { what, value } } if what == "GOOGLE_CLOUD_LOCATION" && value == "asia-south1"), "{elsewhere:?}");
        // Settings it can't parse are said, not guessed past.
        let broken = of("antigravity", &[], None, &[(settings, "{not json")]);
        assert!(matches!(&broken, Reach::Whole { why: Whole::Unread { file, .. } } if file == settings), "{broken:?}");
    }

    /// Step 17 part 6: Goose keeps its provider's hosts, its environment's over its config's, and
    /// TypeSafe's where auto effort is on; a provider its image names no hosts for, or none, keeps the
    /// image whole, saying so.
    #[test]
    fn s17_a_goose_run_keeps_its_providers_hosts() {
        let config = "/home/h/.config/goose/config.yaml";
        let from_config = of("goose", &[], None, &[(config, "GOOSE_PROVIDER: anthropic\nGOOSE_AUTO_EFFORT_ENABLED: true\n")]);
        assert_eq!(narrowed(&from_config).0, ["anthropic", "auto-effort"]);
        let from_env = of("goose", &[("GOOSE_PROVIDER", "groq")], None, &[(config, "GOOSE_PROVIDER: anthropic\n")]);
        assert_eq!(narrowed(&from_env), (vec!["groq".to_owned()], row("goose", "groq")));
        assert!(matches!(of("goose", &[], None, &[]), Reach::Whole { why: Whole::Untold { .. } }));
        assert!(matches!(of("goose", &[("GOOSE_PROVIDER", "mine")], None, &[]), Reach::Whole { why: Whole::Unnamed { .. } }));
        // Where its environment roots Goose's files (kiln's image does), its config is there, not in XDG's place.
        let root = [("GOOSE_PATH_ROOT", "/home/h/.local/share/goose")];
        let rooted = of("goose", &root, None, &[("/home/h/.local/share/goose/config/config.yaml", "GOOSE_PROVIDER: openai\n"), (config, "GOOSE_PROVIDER: anthropic\n")]);
        assert_eq!(narrowed(&rooted).0, ["openai"]);
        assert!(matches!(of("goose", &root, None, &[(config, "GOOSE_PROVIDER: anthropic\n")]), Reach::Whole { why: Whole::Untold { .. } }), "XDG's config isn't the rooted Goose's");
    }

    /// Step 17 part 6: opencode keeps its catalog's and its model's provider's hosts: its session's
    /// model first, then its project's config, then its own.
    #[test]
    fn s17_an_opencode_run_keeps_its_models_providers_hosts() {
        let session = of("opencode", &[], Some("anthropic/claude-sonnet"), &[("/p/opencode.json", "{\"model\": \"openai/gpt-5\"}")]);
        assert_eq!(narrowed(&session).0, ["anthropic"]);
        let project = of("opencode", &[], None, &[("/p/opencode.jsonc", "{\n  // the project's\n  \"model\": \"openai/gpt-5\",\n}"), ("/home/h/.config/opencode/opencode.json", "{\"model\": \"anthropic/x\"}")]);
        assert_eq!(narrowed(&project).0, ["openai"]);
        let mine = of("opencode", &[], None, &[("/home/h/.config/opencode/opencode.json", "{\"model\": \"anthropic/x\"}")]);
        let mut hosts = row("opencode", "anthropic");
        hosts.sort();
        hosts.dedup();
        assert_eq!(narrowed(&mine), (vec!["anthropic".to_owned()], hosts));
        assert!(matches!(of("opencode", &[], None, &[]), Reach::Whole { why: Whole::Untold { .. } }));
        assert!(matches!(of("opencode", &[], Some("gpt-5"), &[]), Reach::Whole { why: Whole::Unnamed { .. } }), "a model that names no provider");
    }

    /// Step 17 part 6: pi keeps pi.dev's and its default provider's hosts; a harness BY_USE has no
    /// table for serves one use, its image whole and no fallback.
    #[test]
    fn s17_a_pi_run_keeps_its_default_providers_hosts_and_one_use_images_stay_whole() {
        let pi = of("pi", &[], None, &[("/home/h/.pi/agent/settings.json", "{\"defaultProvider\": \"anthropic\"}")]);
        let mut hosts = row("pi", "anthropic");
        hosts.sort();
        hosts.dedup();
        assert_eq!(narrowed(&pi), (vec!["anthropic".to_owned()], hosts));
        assert!(matches!(of("pi", &[], None, &[]), Reach::Whole { why: Whole::Untold { .. } }));
        // Its agent directory where its environment names one (kiln's image does).
        let elsewhere = of("pi", &[("PI_CODING_AGENT_DIR", "/v/agent/")], None, &[("/v/agent/settings.json", "{\"defaultProvider\": \"openai\"}")]);
        assert_eq!(narrowed(&elsewhere).0, ["openai"]);
        let claude = of("claude", &[], None, &[]);
        assert_eq!(claude, Reach::Whole { why: Whole::OneUse });
        assert!(!Whole::OneUse.is_fallback() && !Whole::PersonsOwn.is_fallback() && Whole::NoHome.is_fallback());
    }

    /// Step 17 part 6: every use each harness reads names a row of its table: what a run keeps is its
    /// image's, never more (the rows are held to each Agentfile by `grants_tests`).
    #[test]
    fn s17_every_kept_host_is_its_images() {
        for table in BY_USE {
            for (id, _) in table.by {
                let all: Vec<&str> = table.always.iter().chain(table.by.iter().flat_map(|(_, h)| h.iter())).copied().collect();
                for h in row(table.harness, id) {
                    assert!(all.contains(&h.as_str()), "{} {id}: {h}", table.harness);
                }
            }
        }
    }
}
