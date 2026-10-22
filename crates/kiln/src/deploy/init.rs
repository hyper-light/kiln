//! `kiln app init` (docs/deploy-api.md §3.4): an app's chart on kiln's base chart, written into a
//! repository. `charts/<app>/` is an application chart whose one template renders everything its
//! values turn on (`kiln-base.all`), depending on kiln-base in kiln's own registry
//! (`oci://<kiln>/kiln/charts`, where kiln publishes the version it ships), or with kiln-base
//! vendored in its `charts/` when no kiln is named; `deploy/<env>.yaml` is each environment's own
//! values to start from, released with `kiln deploy <app> --to <env> --values deploy/<env>.yaml`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::hub::Hub;

/// Where kiln publishes kiln-base in its own registry.
pub const BASE_REPO: &str = "kiln/charts/kiln-base";

/// The OCI repository a chart's dependency names kiln's charts by: `oci://<host>/kiln/charts`.
pub fn charts_repository(host: &str) -> String {
    format!("oci://{}/kiln/charts", host.trim_end_matches('/'))
}

/// kiln-base, as kiln ships it: each file's path in the chart and its text.
const BASE: &[(&str, &str)] = &[
    ("Chart.yaml", include_str!("../../assets/charts/kiln-base/Chart.yaml")),
    ("templates/_helpers.tpl", include_str!("../../assets/charts/kiln-base/templates/_helpers.tpl")),
    ("templates/_all.tpl", include_str!("../../assets/charts/kiln-base/templates/_all.tpl")),
    ("templates/_config.tpl", include_str!("../../assets/charts/kiln-base/templates/_config.tpl")),
    ("templates/_deployment.tpl", include_str!("../../assets/charts/kiln-base/templates/_deployment.tpl")),
    ("templates/_jobs.tpl", include_str!("../../assets/charts/kiln-base/templates/_jobs.tpl")),
    ("templates/_network.tpl", include_str!("../../assets/charts/kiln-base/templates/_network.tpl")),
    ("templates/_scaling.tpl", include_str!("../../assets/charts/kiln-base/templates/_scaling.tpl")),
    ("templates/_service.tpl", include_str!("../../assets/charts/kiln-base/templates/_service.tpl")),
];

/// The app chart's values, `{{kiln.app}}`, `{{kiln.image}}` and `{{kiln.port}}` to fill in.
const VALUES: &str = include_str!("../../assets/charts/app/values.yaml");

/// The version of kiln-base kiln ships.
pub fn base_version() -> &'static str {
    BASE.first().and_then(|(_, text)| text.lines().find_map(|l| l.strip_prefix("version:"))).map_or("0.0.0", str::trim)
}

/// What `kiln app init` asks for.
pub struct Init<'a> {
    pub app: &'a str,
    /// `ghcr.io/acme/web`; a release fills in its tag or digest.
    pub image: &'a str,
    pub port: u16,
    pub environments: &'a [String],
    /// The kiln whose registry kiln-base comes from (`kiln.example.com`, `127.0.0.1:7070`); `None`:
    /// kiln-base vendored in the chart.
    pub registry: Option<&'a str>,
}

/// The kiln-base dependency of an app's chart: from kiln's registry, or vendored.
fn dependency(registry: Option<&str>) -> String {
    match registry {
        Some(host) => format!(
            "  # kiln's base chart, from kiln's registry: `kiln app init --base` moves it to the version this kiln ships.\n  - name: kiln-base\n    version: {}\n    repository: {}\n",
            base_version(),
            charts_repository(host)
        ),
        None => format!("  # Vendored in charts/kiln-base: a new version of it comes with `kiln app init --base`.\n  - name: kiln-base\n    version: {}\n", base_version()),
    }
}

/// The files `init` writes, by path under the repository, with their text.
pub fn files(i: &Init<'_>) -> Vec<(PathBuf, String)> {
    let chart = Path::new("charts").join(i.app);
    let mut out = vec![
        (
            chart.join("Chart.yaml"),
            format!(
                "apiVersion: v2\nname: {app}\ndescription: {app}, on kiln's base chart (docs/deploy-api.md §3.4)\ntype: application\nversion: 0.1.0\nappVersion: \"0.1.0\"\ndependencies:\n{dependency}",
                app = i.app,
                dependency = dependency(i.registry)
            ),
        ),
        (chart.join("templates/kiln.yaml"), "{{ include \"kiln-base.all\" . }}\n".to_owned()),
        (chart.join("values.yaml"), VALUES.replace("{{kiln.app}}", i.app).replace("{{kiln.image}}", i.image).replace("{{kiln.port}}", &i.port.to_string())),
    ];
    if i.registry.is_none() {
        out.extend(vendored(i.app));
    }
    for env in i.environments {
        let (replicas, autoscale) = if env == "production" || env == "prod" { (3, "true") } else { (1, "false") };
        out.push((
            Path::new("deploy").join(format!("{env}.yaml")),
            format!(
                "# {app} in {env}: laid over charts/{app}/values.yaml. Release it with\n#   kiln deploy {app} --to {env} --values deploy/{env}.yaml\nreplicaCount: {replicas}\nautoscaling:\n  enabled: {autoscale}\npdb:\n  enabled: {pdb}\n",
                app = i.app,
                pdb = replicas > 1
            ),
        ));
    }
    out
}

/// kiln-base's files vendored in an app's chart (`charts/<app>/charts/kiln-base/…`).
pub fn vendored(app: &str) -> Vec<(PathBuf, String)> {
    let dir = Path::new("charts").join(app).join("charts/kiln-base");
    BASE.iter().map(|(path, text)| (dir.join(path), (*text).to_owned())).collect()
}

/// An app chart's `Chart.yaml` with its kiln-base dependency at the version this kiln ships, from
/// `repository` (`None`: vendored, no repository); the rest of it as it was. `None`: it doesn't
/// depend on kiln-base.
pub fn rebase(text: &str, repository: Option<&str>) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    // The dependency's entry: `- name: kiln-base`, and its lines indented under it.
    let start = lines.iter().position(|l| l.trim_start().strip_prefix("- ").map(str::trim_start) == Some("name: kiln-base"))?;
    let item = lines.get(start).map_or(0, |l| l.len() - l.trim_start().len());
    let end = lines.iter().enumerate().skip(start + 1).find(|(_, l)| !l.trim().is_empty() && l.len() - l.trim_start().len() <= item).map_or(lines.len(), |(i, _)| i);
    let pad = " ".repeat(item + 2);
    let mut out: Vec<String> = lines.iter().take(start + 1).map(|l| (*l).to_owned()).collect();
    for l in lines.iter().take(end).skip(start + 1) {
        let key = l.trim_start();
        if !(key.starts_with("version:") || key.starts_with("repository:")) {
            out.push((*l).to_owned());
        }
    }
    out.push(format!("{pad}version: {}", base_version()));
    if let Some(r) = repository {
        out.push(format!("{pad}repository: {r}"));
    }
    out.extend(lines.iter().skip(end).map(|l| (*l).to_owned()));
    Some(out.join("\n") + "\n")
}

/// kiln-base packaged as `helm package` makes it: a gzipped tar of `kiln-base/<file>`, the same
/// bytes every time (no times or owners, every mode 0644), so its digest is its version's.
pub fn base_package() -> anyhow::Result<Vec<u8>> {
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    tar.mode(tar::HeaderMode::Deterministic);
    for (path, text) in BASE {
        let mut h = tar::Header::new_ustar();
        h.set_size(u64::try_from(text.len()).unwrap_or(u64::MAX));
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut h, format!("kiln-base/{path}"), text.as_bytes())?;
    }
    Ok(tar.into_inner()?.finish()?)
}

/// kiln-base's `Chart.yaml`, as JSON: an OCI chart's config.
pub fn base_meta() -> anyhow::Result<serde_json::Value> {
    Ok(serde_yaml::from_str(BASE.first().map_or("", |(_, text)| *text))?)
}

/// Where kiln keeps charts it has itself, packaged (`kiln-base-<version>.tgz`), for KilnCD to lay
/// into a chart that depends on one rather than pull it.
pub fn local_charts(cfg: &crate::config::Config) -> PathBuf {
    cfg.data("charts")
}

/// Publishes kiln-base as this kiln ships it: in kiln's own registry ([`BASE_REPO`] at its version,
/// as `helm push` would; one there under that version is kept), and packaged in
/// [`local_charts`]. The server's, once at start.
pub async fn publish_base(hub: Arc<Hub>) -> anyhow::Result<()> {
    let (tgz, meta, version) = (base_package()?, base_meta()?, base_version());
    let (dir, bytes) = (local_charts(&hub.cfg), tgz.clone());
    crate::rt::blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let (file, part) = (dir.join(format!("kiln-base-{version}.tgz")), dir.join(format!(".kiln-base-{version}.tgz.part")));
        std::fs::write(&part, bytes)?;
        std::fs::rename(part, file)
    })
    .await??;
    let digest = crate::ci::registry::put_chart(&hub, BASE_REPO, version, &tgz, &meta).await?;
    tracing::info!(version, digest = %digest, "kiln-base is published in kiln's registry");
    Ok(())
}

/// Writes `files` under `root`: none already there is changed unless `force`. The paths written,
/// and those left as they were.
pub fn write(root: &Path, files: &[(PathBuf, String)], force: bool) -> std::io::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (mut wrote, mut kept) = (Vec::new(), Vec::new());
    for (path, text) in files {
        let at = root.join(path);
        if at.exists() && !force {
            kept.push(path.clone());
            continue;
        }
        if let Some(dir) = at.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&at, text)?;
        wrote.push(path.clone());
    }
    Ok((wrote, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> Init<'static> {
        static ENVS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        Init { app: "web", image: "ghcr.io/acme/web", port: 8080, environments: ENVS.get_or_init(|| vec!["staging".into(), "production".into()]), registry: None }
    }

    #[test]
    fn kiln_base_from_kiln_s_registry_or_vendored() -> anyhow::Result<()> {
        let fs = files(&Init { registry: Some("kiln.example.com"), ..init() });
        let chart = fs.iter().find(|(p, _)| p == Path::new("charts/web/Chart.yaml")).map(|(_, t)| t.clone()).unwrap_or_default();
        assert!(chart.contains("repository: oci://kiln.example.com/kiln/charts") && chart.contains(&format!("version: {}", base_version())), "{chart}");
        assert!(!fs.iter().any(|(p, _)| p.starts_with("charts/web/charts")), "nothing vendored");
        let deps: serde_yaml::Value = serde_yaml::from_str(&chart)?;
        assert_eq!(deps.get("dependencies").and_then(|d| d.get(0)).and_then(|d| d.get("repository")).and_then(serde_yaml::Value::as_str), Some("oci://kiln.example.com/kiln/charts"));
        // Moved between them, the rest of Chart.yaml as it was.
        let mine = "apiVersion: v2\nname: web\nversion: 0.3.0 # mine\ndependencies:\n  - name: redis\n    version: 18.0.0\n    repository: oci://registry-1.docker.io/bitnamicharts\n  - name: kiln-base\n    version: 0.0.1\n    alias: base\nannotations:\n  team: shop\n";
        let moved = rebase(mine, Some("oci://127.0.0.1:7070/kiln/charts")).unwrap_or_default();
        let v: serde_yaml::Value = serde_yaml::from_str(&moved)?;
        let base = v.get("dependencies").and_then(|d| d.get(1)).cloned().unwrap_or_default();
        assert_eq!(base.get("version").and_then(serde_yaml::Value::as_str), Some(base_version()), "{moved}");
        assert_eq!(base.get("repository").and_then(serde_yaml::Value::as_str), Some("oci://127.0.0.1:7070/kiln/charts"));
        assert_eq!(base.get("alias").and_then(serde_yaml::Value::as_str), Some("base"), "its other keys kept");
        assert!(moved.contains("version: 0.3.0 # mine") && moved.contains("version: 18.0.0") && moved.contains("team: shop"), "{moved}");
        let back = rebase(&moved, None).unwrap_or_default();
        assert!(!back.contains("127.0.0.1"), "vendored again: no repository: {back}");
        assert!(rebase("apiVersion: v2\nname: web\n", None).is_none(), "no kiln-base, nothing to move");
        Ok(())
    }

    #[test]
    fn kiln_base_packaged_the_same_every_time() -> anyhow::Result<()> {
        let (a, b) = (base_package()?, base_package()?);
        assert_eq!(a, b, "the same bytes, so the same digest");
        let mut names = Vec::new();
        for entry in tar::Archive::new(flate2::read::GzDecoder::new(a.as_slice())).entries()? {
            let entry = entry?;
            names.push(entry.path()?.to_string_lossy().into_owned());
            assert_eq!(entry.header().mode()?, 0o644);
        }
        assert!(names.contains(&"kiln-base/Chart.yaml".to_owned()) && names.contains(&"kiln-base/templates/_all.tpl".to_owned()), "{names:?}");
        assert_eq!(base_meta()?.get("version").and_then(serde_json::Value::as_str), Some(base_version()));
        assert_eq!(base_meta()?.get("type").and_then(serde_json::Value::as_str), Some("library"));
        Ok(())
    }

    #[test]
    fn an_app_chart_on_the_base_chart_and_each_environments_values() {
        let fs = files(&init());
        let get = |p: &str| fs.iter().find(|(path, _)| path == Path::new(p)).map(|(_, t)| t.as_str());
        assert!(get("charts/web/Chart.yaml").is_some_and(|t| t.contains("name: web") && t.contains(&format!("version: {}", base_version()))));
        assert_eq!(get("charts/web/templates/kiln.yaml"), Some("{{ include \"kiln-base.all\" . }}\n"));
        let values = get("charts/web/values.yaml").unwrap_or_default();
        assert!(values.contains("repository: ghcr.io/acme/web") && values.contains("port: 8080") && values.contains("mountPath: /etc/web"));
        assert!(!values.contains("{{kiln."), "every placeholder filled");
        assert!(get("charts/web/charts/kiln-base/templates/_all.tpl").is_some_and(|t| t.contains("define \"kiln-base.all\"")));
        assert!(get("deploy/production.yaml").is_some_and(|t| t.contains("replicaCount: 3") && t.contains("enabled: true")));
        assert!(get("deploy/staging.yaml").is_some_and(|t| t.contains("replicaCount: 1")));
        assert_eq!(base_version(), "0.1.3");
    }

    #[test]
    fn what_is_there_already_is_kept_unless_forced() -> std::io::Result<()> {
        let tmp = tempfile::tempdir()?;
        std::fs::create_dir_all(tmp.path().join("deploy"))?;
        std::fs::write(tmp.path().join("deploy/staging.yaml"), "replicaCount: 7\n")?;
        let fs = files(&init());
        let (wrote, kept) = write(tmp.path(), &fs, false)?;
        assert_eq!(kept, vec![PathBuf::from("deploy/staging.yaml")]);
        assert_eq!(wrote.len(), fs.len() - 1);
        assert_eq!(std::fs::read_to_string(tmp.path().join("deploy/staging.yaml"))?, "replicaCount: 7\n");
        let (again, _) = write(tmp.path(), &fs, true)?;
        assert_eq!(again.len(), fs.len());
        // Where helm is installed, the chart lints and renders.
        let lint = std::process::Command::new("helm").args(["lint", "--quiet"]).arg(tmp.path().join("charts/web")).output();
        if let Ok(out) = lint {
            assert!(out.status.success(), "helm lint: {}", String::from_utf8_lossy(&out.stdout));
            let out = std::process::Command::new("helm").arg("template").arg("web").arg(tmp.path().join("charts/web")).output()?;
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && text.contains("kind: Deployment") && text.contains("image: ghcr.io/acme/web:0.1.0"), "{text}");
            // A rollout keeps what ran serving until the new pods are ready, and says when it's stuck.
            assert!(text.contains("maxUnavailable: 0") && text.contains("progressDeadlineSeconds: 300"), "{text}");
            assert!(!text.contains("web-kiln"), "no secrets unless asked for");
            // The app's secrets from kiln: the Secret KilnCD delivers, as env, there or not.
            let out = std::process::Command::new("helm").args(["template", "web", "--set", "kilnSecrets=true"]).arg(tmp.path().join("charts/web")).output()?;
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && text.contains("name: web-kiln") && text.contains("optional: true"), "{text}");
        }
        Ok(())
    }
}
