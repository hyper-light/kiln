//! The release repository's layout (docs/deploy-api.md §6):
//!
//! ```text
//! <root>/<project>/<app>/app.yaml               the app's package and defaults
//! <root>/<project>/<app>/<env>/release.json     what this environment runs: kiln writes it
//! <root>/<project>/<app>/<env>/values.yaml      this environment's values
//! ```
//!
//! One release, one commit, whose message names it and whose trailers say who asked
//! (`Kiln-Release`, `Kiln-Requested-By`).

use std::collections::BTreeMap;

use serde_json::Value;

use super::model::{App, Artifact, Kind, Package, Release, ReleaseFile, RepoRef, SCHEMA};

/// The trailer that names the release a commit holds.
pub const RELEASE_TRAILER: &str = "Kiln-Release";
/// The trailer that names who asked for it.
pub const REQUESTED_BY_TRAILER: &str = "Kiln-Requested-By";

fn join(root: &str, rest: &str) -> String {
    let root = root.trim_matches('/');
    if root.is_empty() { rest.to_owned() } else { format!("{root}/{rest}") }
}

/// The directory of an app.
pub fn app_dir(repo: &RepoRef, app: &App) -> String {
    join(&repo.root, &format!("{}/{}", app.project, app.name))
}

/// The directory of a registration.
pub fn registration_dir(repo: &RepoRef, app: &App, env: &str) -> String {
    format!("{}/{env}", app_dir(repo, app))
}

pub fn app_file(repo: &RepoRef, app: &App) -> String {
    format!("{}/app.yaml", app_dir(repo, app))
}

pub fn release_file(repo: &RepoRef, app: &App, env: &str) -> String {
    format!("{}/release.json", registration_dir(repo, app, env))
}

pub fn values_file(repo: &RepoRef, app: &App, env: &str) -> String {
    format!("{}/values.yaml", registration_dir(repo, app, env))
}

/// `app.yaml`: the app as the release repository keeps it.
pub fn app_yaml(app: &App) -> anyhow::Result<Vec<u8>> {
    let mut text = String::from("# Written by kiln (docs/deploy-api.md §6). Changes made here in a reviewed commit are releases too.\n");
    text.push_str(&serde_yaml::to_string(app)?);
    Ok(text.into_bytes())
}

/// `release.json` for a release, on top of what the registration ran before (`prior`).
pub fn release_json(app: &App, env: &str, r: &Release, prior: Option<&ReleaseFile>, enabled: bool) -> ReleaseFile {
    let keep = |f: &ReleaseFile| (f.version.clone(), f.artifacts.clone());
    let (version, artifacts) = match r.kind {
        Kind::Deploy | Kind::Rollback => (r.version.clone(), r.artifacts.clone()),
        Kind::Config | Kind::Enable | Kind::Disable | Kind::Revert => prior.map(keep).unwrap_or_default(),
    };
    ReleaseFile {
        schema: SCHEMA,
        project: app.project.clone(),
        app: app.name.clone(),
        environment: env.to_owned(),
        release: r.id,
        enabled,
        version,
        package: app.package.clone(),
        artifacts,
        requested_by: r.requested_by.clone(),
        at: rfc3339(r.created_at),
        rollback_of: r.rollback_of,
        secrets: String::new(),
    }
}

/// `release.json` as a registration starts: enabled, by default, once a version lands.
pub fn registered_json(app: &App, env: &str) -> ReleaseFile {
    ReleaseFile {
        schema: SCHEMA,
        project: app.project.clone(),
        app: app.name.clone(),
        environment: env.to_owned(),
        release: 0,
        enabled: false,
        version: None,
        package: app.package.clone(),
        artifacts: BTreeMap::new(),
        requested_by: String::new(),
        at: rfc3339(crate::util::now_unix()),
        rollback_of: None,
        secrets: String::new(),
    }
}

pub fn encode(f: &ReleaseFile) -> anyhow::Result<Vec<u8>> {
    let mut v = serde_json::to_vec_pretty(f)?;
    v.push(b'\n');
    Ok(v)
}

pub fn decode(bytes: &[u8]) -> anyhow::Result<ReleaseFile> {
    Ok(serde_json::from_slice(bytes)?)
}

/// The commit message of a release: what it does, then its trailers.
pub fn message(app: &App, env: &str, r: &Release) -> String {
    let what = match r.kind {
        Kind::Deploy => format!("deploy {} {} to {env}", app.name, r.version.as_deref().unwrap_or("?")),
        Kind::Config => match &r.secrets {
            Some(what) if r.values.is_none() => format!("deliver {}'s secrets in {env}: {what}", app.name),
            _ => format!("change {}'s values in {env}", app.name),
        },
        Kind::Rollback => format!(
            "roll {} in {env} back to {}{}",
            app.name,
            r.version.as_deref().unwrap_or("?"),
            r.rollback_of.map(|to| format!(" (release {to})")).unwrap_or_default()
        ),
        Kind::Enable => format!("enable {} in {env}", app.name),
        Kind::Disable => format!("disable {} in {env}", app.name),
        Kind::Revert => format!("put back what ran before release {} of {} in {env}", r.reverts.unwrap_or_default(), app.name),
    };
    format!("{what} (release {})\n\n{RELEASE_TRAILER}: {}\n{REQUESTED_BY_TRAILER}: {}\n", r.id, r.id, r.requested_by)
}

/// Seconds since the epoch as RFC 3339 (UTC).
pub fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Howard Hinnant's days-to-civil.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Expressions a platform's definition leaves to KilnCD, which fills them as it renders.
pub const PLATFORM_KEEPS: &[&str] = &["git", "artifacts"];

const FILLABLE: &str = "release.version, release.id, release.artifacts.<name>.<repository|tag|digest>, env.name, app.name";

/// Fills `${{ release.version }}`, `${{ release.id }}`,
/// `${{ release.artifacts.<name>.<repository|tag|digest> }}`, `${{ env.name }}` and
/// `${{ app.name }}` in a package's values.
pub fn fill(template: &str, file: &ReleaseFile) -> Result<String, String> {
    fill_keeping(template, file, &[])
}

/// [`fill`], leaving the expressions under `keep` (`git`, `artifacts`) as they are.
fn fill_keeping(template: &str, file: &ReleaseFile, keep: &[&str]) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        out.push_str(rest.get(..start).unwrap_or(""));
        let after = rest.get(start + 3..).unwrap_or("");
        let Some(end) = after.find("}}") else { return Err(format!("{template:?}: `${{{{` without `}}}}`")) };
        let expr = after.get(..end).unwrap_or("").trim();
        if kept(expr, keep) {
            out.push_str(rest.get(start..start + 3 + end + 2).unwrap_or(""));
        } else {
            out.push_str(&lookup(expr, file).ok_or_else(|| format!("{template:?}: kiln can't fill `{expr}` ({FILLABLE})"))?);
        }
        rest = after.get(end + 2..).unwrap_or("");
    }
    out.push_str(rest);
    Ok(out)
}

fn kept(expr: &str, keep: &[&str]) -> bool {
    keep.iter().any(|k| expr == *k || expr.strip_prefix(*k).is_some_and(|rest| rest.starts_with('.')))
}

/// A value with every string's expressions filled ([`fill`]), all the way down, those under
/// `keep` left as they are.
pub fn fill_value(v: &Value, file: &ReleaseFile, keep: &[&str]) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => Value::String(fill_keeping(s, file, keep)?),
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_value(x, file, keep)).collect::<Result<_, _>>()?),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), fill_value(x, file, keep)?))).collect::<Result<_, String>>()?),
        other => other.clone(),
    })
}

/// Whether every expression in a value is one kiln fills, or one under `keep`.
pub fn check_value(v: &Value, keep: &[&str]) -> Result<(), String> {
    match v {
        Value::String(s) => check_keeping(s, keep),
        Value::Array(a) => a.iter().try_for_each(|x| check_value(x, keep)),
        Value::Object(m) => m.values().try_for_each(|x| check_value(x, keep)),
        _ => Ok(()),
    }
}

fn lookup(expr: &str, file: &ReleaseFile) -> Option<String> {
    match expr {
        "env.name" => return Some(file.environment.clone()),
        "app.name" => return Some(file.app.clone()),
        _ => {}
    }
    let path = expr.strip_prefix("release.")?;
    match path.split('.').collect::<Vec<_>>().as_slice() {
        ["version"] => file.version.clone(),
        ["id"] => Some(file.release.to_string()),
        ["artifacts", name, field] => {
            let a: &Artifact = file.artifacts.get(*name)?;
            Some(match *field {
                "repository" => a.repository.clone(),
                "tag" => a.tag.clone(),
                "digest" => a.digest.clone(),
                _ => return None,
            })
        }
        _ => None,
    }
}

/// Whether every `${{ }}` in `template` is one kiln fills, before any release exists.
pub fn check(template: &str) -> Result<(), String> {
    check_keeping(template, &[])
}

fn check_keeping(template: &str, keep: &[&str]) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("${{") {
        let after = rest.get(start + 3..).unwrap_or("");
        let Some(end) = after.find("}}") else { return Err(format!("{template:?}: `${{{{` without `}}}}`")) };
        let expr = after.get(..end).unwrap_or("").trim();
        let ok = kept(expr, keep)
            || matches!(expr, "env.name" | "app.name")
            || match expr.strip_prefix("release.").map(|p| p.split('.').collect::<Vec<_>>()) {
                Some(p) => matches!(p.as_slice(), ["version"] | ["id"] | ["artifacts", _, "repository" | "tag" | "digest"]),
                None => false,
            };
        if !ok {
            return Err(format!("{template:?}: kiln can't fill `{expr}` ({FILLABLE})"));
        }
        rest = after.get(end + 2..).unwrap_or("");
    }
    Ok(())
}

/// A package's `set`, filled for a release.
pub fn filled_set(package: &Package, file: &ReleaseFile) -> Result<BTreeMap<String, String>, String> {
    package.set().iter().map(|(k, v)| Ok((k.clone(), fill(v, file)?))).collect()
}

/// The JSON a client sees for a `release.json` (unknown future keys kept).
pub fn as_value(f: &ReleaseFile) -> Value {
    serde_json::to_value(f).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::model::{Origin, State};

    fn app() -> App {
        App {
            name: "web".into(),
            project: "shop".into(),
            owners: vec!["shop".into()],
            package: Package::Helm {
                repo: "oci://ghcr.io/acme/charts".into(),
                chart: "web".into(),
                chart_version: None,
                set: BTreeMap::from([("image.tag".into(), "${{ release.artifacts.image.tag }}".into()), ("kiln.release".into(), "${{ release.id }}".into())]),
            },
            namespace: None,
            properties: BTreeMap::new(),
            template: None, contacts: Default::default(), source: None, archived: false,
        }
    }

    fn release(kind: Kind, version: Option<&str>) -> Release {
        Release {
            id: 42,
            app: "web".into(),
            environment: "staging".into(),
            kind,
            state: State::Committing,
            origin: Origin::Api,
            version: version.map(str::to_owned),
            artifacts: BTreeMap::from([("image".into(), Artifact { repository: "ghcr.io/acme/web".into(), tag: "1.4.2".into(), digest: "sha256:9f".into() })]),
            values: None,
            commit: None,
            requested_by: "ada".into(),
            message: None,
            rollback_of: None,
            rolled_back_by: None,
            keep: false,
            provider: Value::Null,
            gates: Vec::new(),
            approvals: Vec::new(),
            promoted_from: None,
            idempotency_key: None,
            created_at: 1_792_356_000,
            committed_at: None,
            ended_at: None,
            secrets: None,
            no_revert: false,
            reverts: None,
            reverted_by: None,
            irreversible: None,
        }
    }

    #[test]
    fn paths_follow_the_layout() {
        let repo = RepoRef { root: "/apps/".into(), ..RepoRef::default() };
        assert_eq!(release_file(&repo, &app(), "staging"), "apps/shop/web/staging/release.json");
        assert_eq!(values_file(&RepoRef::default(), &app(), "staging"), "shop/web/staging/values.yaml");
        assert_eq!(app_file(&RepoRef::default(), &app()), "shop/web/app.yaml");
    }

    #[test]
    fn a_config_release_keeps_the_version_a_deploy_changes_it() -> Result<(), String> {
        let deploy = release_json(&app(), "staging", &release(Kind::Deploy, Some("1.4.2")), None, true);
        assert_eq!((deploy.version.as_deref(), deploy.release, deploy.enabled), (Some("1.4.2"), 42, true));
        let config = release_json(&app(), "staging", &release(Kind::Config, None), Some(&deploy), true);
        assert_eq!(config.version.as_deref(), Some("1.4.2"), "the version stays");
        assert_eq!(filled_set(&app().package, &deploy)?.get("image.tag").map(String::as_str), Some("1.4.2"));
        assert_eq!(filled_set(&app().package, &deploy)?.get("kiln.release").map(String::as_str), Some("42"));
        assert!(fill("${{ release.nope }}", &deploy).is_err());
        assert_eq!(fill("v${{release.version}}-x", &deploy)?, "v1.4.2-x");
        Ok(())
    }

    #[test]
    fn messages_carry_the_release_and_who_asked() {
        let m = message(&app(), "staging", &release(Kind::Deploy, Some("1.4.2")));
        assert!(m.starts_with("deploy web 1.4.2 to staging (release 42)\n\n"));
        assert!(m.contains("Kiln-Release: 42\n") && m.contains("Kiln-Requested-By: ada\n"));
    }

    #[test]
    fn a_platform_s_definition_is_filled_but_for_what_kilncd_fills() -> Result<(), String> {
        let f = release_json(&app(), "staging", &release(Kind::Deploy, Some("1.4.2")), None, true);
        let spec = serde_json::json!({ "name": "${{ app.name }}-${{ env.name }}", "tags": ["v${{ release.version }}", "${{ artifacts.zip }}"], "sha": "${{ git.sha }}" });
        let filled = fill_value(&spec, &f, PLATFORM_KEEPS)?;
        assert_eq!(filled, serde_json::json!({ "name": "web-staging", "tags": ["v1.4.2", "${{ artifacts.zip }}"], "sha": "${{ git.sha }}" }));
        assert!(check_value(&spec, PLATFORM_KEEPS).is_ok());
        assert!(check_value(&serde_json::json!({ "x": ["${{ secrets.token }}"] }), PLATFORM_KEEPS).is_err(), "nothing fills secrets");
        assert!(fill_value(&serde_json::json!("${{ git.sha }}"), &f, &[]).is_err(), "kept only where kept");
        Ok(())
    }

    #[test]
    fn times_read_as_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_356_000), "2026-10-18T20:40:00Z");
    }
}
