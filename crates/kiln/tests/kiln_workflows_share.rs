//! W28-R77, R78 (docs/ci.md): kiln's own workflows build the UI once per ui/ tree for ci, release
//! and the image, and x86_64-unknown-linux-musl once per commit for release and the image. Each
//! job's shared step is keyed here as its runner would key it, on the platform that job runs on,
//! with its job's `needs` and variables: the keys are one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::ir::{Step, Workflow};
use kiln_ci::work::key::{self, Platform};
use kiln_ci::work::memo::{self, Definition, Earlier, Facts, Memo};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn workflow(file: &str) -> anyhow::Result<Workflow> {
    let src = std::fs::read_to_string(root().join(".kiln").join(file))?;
    let (w, d) = kiln_ci::wf::parse_workflow(&format!(".kiln/{file}"), &src, Dialect::Kiln);
    anyhow::ensure!(d.iter().all(|x| !x.is_error()), "{file}: {d:?}");
    w.ok_or_else(|| anyhow::anyhow!("{file} doesn't read"))
}

/// The step named `name` of `job` in `file`, and its memo steps before it.
fn step(file: &str, job: &str, name: &str) -> anyhow::Result<(Step, Vec<Step>)> {
    let w = workflow(file)?;
    let j = w.jobs.into_iter().find(|j| j.id == job).ok_or_else(|| anyhow::anyhow!("{file}: no job {job}"))?;
    let at = j.steps.iter().position(|s| s.name.as_ref().is_some_and(|n| n.src == name)).ok_or_else(|| anyhow::anyhow!("{file}/{job}: no step {name}"))?;
    let before = j.steps.iter().take(at).filter(|s| s.kiln.memo.is_some()).cloned().collect();
    Ok((j.steps.get(at).cloned().ok_or_else(|| anyhow::anyhow!("step"))?, before))
}

fn memo_of(s: &Step) -> anyhow::Result<Memo> {
    let m = s.kiln.memo.as_ref().ok_or_else(|| anyhow::anyhow!("no memo"))?;
    Memo::from_yval(m).map_err(|e| anyhow::anyhow!(e))?.ok_or_else(|| anyhow::anyhow!("memo off"))
}

/// A job as its runner sees it: where it runs, what its job declares, what it needs.
struct Where {
    os: &'static str,
    arch: &'static str,
    executor: &'static str,
    image: Option<&'static str>,
    node: &'static str,
    rustc: &'static str,
    job_env: &'static [(&'static str, &'static str)],
    needs: Option<&'static str>,
}

/// The step's key on `at`: its definition as the runner makes it, every variable the leg has, its
/// platform and toolchains there, its inputs (one tree for the commit), earlier memo steps.
fn key_on(s: &Step, at: &Where, earlier: &[Earlier]) -> anyhow::Result<String> {
    let m = memo_of(s)?;
    let definition = Definition { run: s.run.as_ref().map(|t| t.src.clone()), shell: Some("bash".into()), outputs: m.outputs.clone(), ..Definition::default() };
    let mut env: BTreeMap<String, String> = at.job_env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    env.insert("PATH".into(), format!("/{}/bin", at.os));
    env.insert("KILN_SHA".into(), "c0ffee".into());
    env.insert("GITHUB_JOB".into(), format!("{}-{}", at.os, at.executor));
    let declared: BTreeSet<String> = env.keys().cloned().collect();
    let mut toolchains = BTreeMap::new();
    for (tool, precision) in &m.tools {
        let text = if tool == "node" { at.node } else { at.rustc };
        toolchains.insert(tool.clone(), memo::identity(*precision, text).ok_or_else(|| anyhow::anyhow!("{tool}: no version"))?);
    }
    let platform = Platform { os: at.os.into(), arch: at.arch.into(), libc: String::new(), executor: at.executor.into(), image: at.image.map(str::to_owned), toolchains };
    let inputs = m.inputs.iter().map(|p| (p.clone(), format!("tree-of-{p}"))).collect();
    let upstream = at.needs.map(|n| BTreeMap::from([("needs".to_owned(), n.to_owned())])).unwrap_or_default();
    let f = Facts { definition, env, declared, platform, inputs, upstream, trusted: true };
    Ok(key::weak(&memo::portable_unit(&f, &m, earlier)).hex())
}

const K8S: Where = Where { os: "linux", arch: "x86_64", executor: "k8s", image: Some("sha256:node24bookworm"), node: "v24.11.1", rustc: "", job_env: &[], needs: None };
const ADACACHY: Where = Where { os: "linux", arch: "x86_64", executor: "host", image: None, node: "v24.21.0", rustc: "rustc 1.99.1\nhost: x86_64-unknown-linux-gnu", job_env: &[], needs: None };
const UI_CHECKS: Where = Where { job_env: &[("CARGO_BUILD_JOBS", "4"), ("CARGO_PROFILE_DEV_DEBUG", "0")], needs: Some("rust,ui"), ..ADACACHY };
const OTHER_LINUX: Where = Where { node: "v24.9.0", ..ADACACHY };
const MAC: Where = Where { os: "macos", arch: "aarch64", executor: "host", image: None, node: "v24.21.0", rustc: "rustc 1.99.1\nhost: aarch64-apple-darwin", job_env: &[], needs: None };

/// W28-AC77: every job that builds the UI keys its UI step alike: one execution per ui/ tree.
#[test]
fn w28_ac77_one_ui_build_per_tree_across_ci_release_and_image() -> anyhow::Result<()> {
    let jobs: [(&str, &str, &Where); 7] = [
        ("ci.yaml", "ui", &K8S),
        ("ci.yaml", "ui-checks", &UI_CHECKS),
        ("ci.yaml", "release-binary", &Where { needs: Some("rust,ui"), ..OTHER_LINUX }),
        ("release.yaml", "linux", &OTHER_LINUX),
        ("release.yaml", "windows", &ADACACHY),
        ("release.yaml", "macos", &MAC),
        ("image.yaml", "image", &ADACACHY),
    ];
    let mut keys = BTreeSet::new();
    for (file, job, at) in jobs {
        let (s, before) = step(file, job, "ui")?;
        anyhow::ensure!(before.is_empty(), "{file}/{job}: the UI step is its first memo step");
        keys.insert(key_on(&s, at, &[])?);
    }
    anyhow::ensure!(keys.len() == 1, "distinct UI keys: {keys:?}");
    Ok(())
}

/// W28-AC78: release's linux job and the image key their x86_64 musl step alike: one compilation
/// per commit; another commit or another rustc is another key.
#[test]
fn w28_ac78_one_musl_build_per_commit_for_release_and_image() -> anyhow::Result<()> {
    let ui = Earlier { key: "the-ui-steps-key".into(), outputs: vec!["ui/dist".into()], digest: "ui-digest".into() };
    let (release, rb) = step("release.yaml", "linux", "x86_64 musl")?;
    let (image, ib) = step("image.yaml", "image", "x86_64 musl")?;
    anyhow::ensure!(rb.len() == 1 && ib.len() == 1, "after the shared UI step only");
    let a = key_on(&release, &OTHER_LINUX, std::slice::from_ref(&ui))?;
    anyhow::ensure!(a == key_on(&image, &ADACACHY, std::slice::from_ref(&ui))?, "release and image key the build alike");
    anyhow::ensure!(a != key_on(&image, &Where { rustc: "rustc 1.100.0\nhost: x86_64-unknown-linux-gnu", ..ADACACHY }, std::slice::from_ref(&ui))?, "another rustc is another build");
    anyhow::ensure!(a != key_on(&image, &ADACACHY, &[Earlier { digest: "another-ui".into(), ..ui }])?, "another UI is another build");
    let text = release.run.as_ref().map(|t| t.src.clone()).unwrap_or_default();
    anyhow::ensure!(text.contains("KILN_COMMIT=$KILN_SHA"), "the build names its commit, which the binary embeds: {text}");
    Ok(())
}
