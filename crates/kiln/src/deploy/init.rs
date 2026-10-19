//! `kiln app init` (docs/deploy-api.md §3.4): an app's chart on kiln's base chart, written into a
//! repository. `charts/<app>/` is an application chart whose one template renders everything its
//! values turn on (`kiln-base.all`), with kiln-base vendored in its `charts/`; `deploy/<env>.yaml`
//! is each environment's own values to start from, released with
//! `kiln deploy <app> --to <env> --values deploy/<env>.yaml`.

use std::path::{Path, PathBuf};

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
}

/// The files `init` writes, by path under the repository, with their text.
pub fn files(i: &Init<'_>) -> Vec<(PathBuf, String)> {
    let chart = Path::new("charts").join(i.app);
    let mut out = vec![
        (
            chart.join("Chart.yaml"),
            format!(
                "apiVersion: v2\nname: {app}\ndescription: {app}, on kiln's base chart (docs/deploy-api.md §3.4)\ntype: application\nversion: 0.1.0\nappVersion: \"0.1.0\"\ndependencies:\n  # Vendored in charts/kiln-base: a new version of it comes with `kiln app init --base`.\n  - name: kiln-base\n    version: {base}\n",
                app = i.app,
                base = base_version()
            ),
        ),
        (chart.join("templates/kiln.yaml"), "{{ include \"kiln-base.all\" . }}\n".to_owned()),
        (chart.join("values.yaml"), VALUES.replace("{{kiln.app}}", i.app).replace("{{kiln.image}}", i.image).replace("{{kiln.port}}", &i.port.to_string())),
    ];
    out.extend(BASE.iter().map(|(path, text)| (chart.join("charts/kiln-base").join(path), (*text).to_owned())));
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
        Init { app: "web", image: "ghcr.io/acme/web", port: 8080, environments: ENVS.get_or_init(|| vec!["staging".into(), "production".into()]) }
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
        assert_eq!(base_version(), "0.1.0");
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
        }
        Ok(())
    }
}
