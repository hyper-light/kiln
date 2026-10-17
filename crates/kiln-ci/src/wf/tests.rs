//! W02's unit tests: the scenarios (W02-S01 to S05), v1 forms (R06), overlays (AC05), file
//! kinds and precedence (R01, R02, AC08), schemas (R09). The corpora run in `tests/wf_corpus.rs`.

use super::*;
use crate::expr::Dialect;

fn gha(src: &str) -> (Option<Workflow>, Vec<Diag>) {
    parse_workflow(".github/workflows/ci.yml", src, Dialect::Gha)
}

fn kiln(src: &str) -> (Option<Workflow>, Vec<Diag>) {
    parse_workflow(".kiln/ci.yaml", src, Dialect::Kiln)
}

fn codes(d: &[Diag]) -> Vec<&'static str> {
    d.iter().map(|x| x.code).collect()
}

fn errors(d: &[Diag]) -> Vec<String> {
    d.iter().filter(|x| x.is_error()).map(|x| format!("{} {}:{} {}", x.code, x.range.start.line, x.range.start.col, x.message)).collect()
}

#[test]
fn w02_s01_a_plain_github_workflow() {
    let (wf, d) = gha("on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo hi\n");
    assert!(errors(&d).is_empty(), "{:?}", errors(&d));
    assert!(codes(&d).contains(&"KCI6002"), "tag, not SHA: info");
    let wf = wf.unwrap_or_default();
    assert_eq!(wf.jobs.len(), 1);
    assert_eq!(wf.on.first().map(|t| t.event.as_str()), Some("push"));
    assert!(!wf.checkout, "GHA workflows never check out implicitly");
}

#[test]
fn w02_s02_run_and_uses_together() {
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo\n        uses: actions/checkout@v4\n");
    let e = d.iter().find(|x| x.code == "KCI1012");
    assert_eq!(e.map(|x| x.range.start.line), Some(6), "{d:?}");
}

#[test]
fn w02_s03_secrets_not_available_in_job_if() {
    let (_, d) = gha("on: push\njobs:\n  a:\n    if: ${{ secrets.X == '' }}\n    runs-on: x\n    steps: [{run: echo}]\n");
    let e = d.iter().find(|x| x.code == "KCI3001");
    assert!(e.is_some(), "{d:?}");
    assert_eq!(e.map(|x| (x.range.start.line, x.range.start.col)), Some((4, 13)), "points at `secrets`");
}

/// `src` with a diagnostic's fix applied (edits are 1-based line and column ranges of the
/// original text, as an editor applies them).
fn apply_fix(src: &str, d: &Diag) -> String {
    let lines: Vec<&str> = src.split('\n').collect();
    let offset = |p: crate::wf::diag::Pos| -> usize {
        let line_start: usize = lines.iter().take(p.line as usize - 1).map(|l| l.len() + 1).sum();
        let line = lines.get(p.line as usize - 1).copied().unwrap_or_default();
        line_start + line.char_indices().nth(p.col as usize - 1).map_or(line.len(), |(i, _)| i)
    };
    let mut edits = d.fix.as_ref().map(|f| f.edits.clone()).unwrap_or_default();
    edits.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.col)));
    let mut out = src.to_owned();
    for e in edits {
        out.replace_range(offset(e.range.start)..offset(e.range.end), &e.text);
    }
    out
}

fn injection(d: &[Diag]) -> Option<&Diag> {
    d.iter().find(|x| x.code == "KCI6004")
}

#[test]
fn w02_s05_untrusted_input_in_run() {
    let src = "on: pull_request\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo \"${{ github.event.pull_request.title }}\"\n";
    let (_, d) = gha(src);
    let fixed = injection(&d).map(|x| apply_fix(src, x)).unwrap_or_default();
    assert_eq!(
        fixed,
        "on: pull_request\njobs:\n  a:\n    runs-on: x\n    steps:\n      - env:\n          PULL_REQUEST_TITLE: ${{ github.event.pull_request.title }}\n        run: echo \"${PULL_REQUEST_TITLE}\"\n"
    );
    let (wf, d) = gha(&fixed);
    assert!(errors(&d).is_empty() && !codes(&d).contains(&"KCI6004"), "{d:?}");
    let step = wf.as_ref().and_then(|w| w.jobs.first()).and_then(|j| j.steps.first()).map(|s| s.env.clone()).unwrap_or_default();
    assert_eq!(step.first().map(|(k, v)| (k.as_str(), v.src.as_str())), Some(("PULL_REQUEST_TITLE", "${{ github.event.pull_request.title }}")));
}

#[test]
fn w02_s05_fix_quotes_for_the_shell() {
    // A block scalar, single and no quotes, an existing env, the same value twice.
    let src = "on: issues\njobs:\n  a:\n    runs-on: x\n    steps:\n      - name: greet\n        env:\n          A: b\n        run: |\n          echo '${{ github.head_ref }}' # it's\n          echo ${{ github.event.issue.title }} ${{ github.event.issue.title }}\n";
    let (_, d) = gha(src);
    assert_eq!(
        injection(&d).map(|x| apply_fix(src, x)).as_deref(),
        Some("on: issues\njobs:\n  a:\n    runs-on: x\n    steps:\n      - name: greet\n        env:\n          HEAD_REF: ${{ github.head_ref }}\n          ISSUE_TITLE: ${{ github.event.issue.title }}\n          A: b\n        run: |\n          echo ''\"${HEAD_REF}\"'' # it's\n          echo \"${ISSUE_TITLE}\" \"${ISSUE_TITLE}\"\n")
    );
    // PowerShell on Windows.
    let src = "on: issues\njobs:\n  a:\n    runs-on: windows-latest\n    steps:\n      - run: Write-Host \"${{ github.event.issue.body }}\"\n";
    let (_, d) = gha(src);
    assert!(injection(&d).map(|x| apply_fix(src, x)).is_some_and(|f| f.contains("ISSUE_BODY: ${{ github.event.issue.body }}\n        run: Write-Host \"$env:ISSUE_BODY\"")));
    // A name that's taken gets a suffix; an element of an array matches its `*` pattern.
    let src = "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: echo ${{ github.event.commits[0].message }}\n        env:\n          COMMITS_0_MESSAGE: x\n";
    let (_, d) = gha(src);
    assert!(injection(&d).map(|x| apply_fix(src, x)).is_some_and(|f| f.contains("COMMITS_0_MESSAGE_2: ${{ github.event.commits[0].message }}")));
    // No fix where the edit can't be made safely: a flow-style step, an unknown shell.
    for src in [
        "on: issues\njobs:\n  a:\n    runs-on: x\n    steps: [{run: \"echo ${{ github.event.issue.title }}\"}]\n",
        "on: issues\njobs:\n  a:\n    runs-on: x\n    steps:\n      - shell: python\n        run: print(\"${{ github.event.issue.title }}\")\n",
    ] {
        let (_, d) = gha(src);
        assert!(injection(&d).is_some_and(|x| x.fix.is_none()), "{src}");
    }
}

#[test]
fn github_rules_are_strict() {
    let (_, d) = gha("on: push\njobs:\n  a:\n    run-on: x\n    steps: [{run: echo}]\n");
    let e = d.iter().find(|x| x.code == "KCI1010").map(|x| x.message.clone()).unwrap_or_default();
    assert!(e.contains("did you mean `runs-on`"), "{e}");
    assert!(codes(&d).contains(&"KCI1014"), "runs-on missing");
    let (_, d) = gha("on: pushh\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(codes(&d).contains(&"KCI1018"));
    let (_, d) = gha("on:\n  pull_request:\n    types: [opend]\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(d.iter().any(|x| x.code == "KCI1015" && x.message.contains("opened")), "{d:?}");
    let (_, d) = gha("on: push\npermissions:\n  content: read\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(d.iter().any(|x| x.code == "KCI1015" && x.message.contains("contents")));
    let (_, d) = gha("on: push\njobs:\n  a:\n    needs: [b]\n    runs-on: x\n    steps: [{run: echo}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(codes(&d).contains(&"KCI4002"), "cycle");
    let (_, d) = gha("on: push\njobs:\n  a:\n    needs: [bild]\n    runs-on: x\n    steps: [{run: echo}]\n  build:\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(d.iter().any(|x| x.code == "KCI4001" && x.message.contains("build")));
    let (_, d) = gha("on: push\njobs:\n  call:\n    uses: ./.github/workflows/r.yml\n    runs-on: x\n");
    assert!(codes(&d).contains(&"KCI1016"), "a reusable call can't have runs-on");
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    strategy:\n      matrix:\n        os: []\n    steps: [{run: echo}]\n");
    assert!(codes(&d).contains(&"KCI1017"));
}

#[test]
fn kiln_keys_are_refused_in_github_workflows() {
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    retry: {max: 2}\n    steps: [{run: echo}]\n");
    let e = d.iter().find(|x| x.code == "KCI5002").map(|x| x.message.clone()).unwrap_or_default();
    assert!(e.contains("overlay"), "{e}");
    let (_, d) = gha("on:\n  diff: {}\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(codes(&d).contains(&"KCI5002"));
    let (_, d) = gha("on: push\njobs:\n  a:\n    if: changed('x')\n    runs-on: x\n    steps: [{run: echo}]\n");
    assert!(codes(&d).contains(&"KCI5001"), "{d:?}");
}

#[test]
fn w02_r06_v1_forms() {
    let src = "name: ci\non:\n  push: { branches: [main] }\n  tag: { branches: ['v*'] }\n  schedule: ['0 6 * * *']\njobs:\n  test:\n    toolchain: rust\n    matrix: { profile: [dev, release] }\n    retries: 2\n    artifacts: ['target/**']\n    cache: ['.cargo/registry']\n    exclusive: true\n    if: branch == 'main'\n    steps:\n      - run: cargo test --profile ${{ matrix.profile }}\n        tty: true\n";
    let (wf, d) = kiln(src);
    assert!(errors(&d).is_empty(), "{:?}", errors(&d));
    let wf = wf.unwrap_or_default();
    assert!(wf.checkout, "kiln checks out implicitly");
    assert!(wf.trigger("workflow_dispatch").is_some(), "v1 allows manual runs by default");
    assert!(wf.on.iter().any(|t| t.event == "push" && t.tags.as_deref() == Some(&["v*".to_owned()][..])), "on.tag became push tags");
    assert_eq!(wf.trigger("schedule").map(|t| t.schedules.len()), Some(1));
    let j = wf.job("test").cloned().unwrap_or_default();
    assert!(matches!(j.strategy.as_ref().and_then(|s| s.matrix.as_ref()), Some(ir::Matrix::Spec(m)) if m.axes.len() == 1));
    assert_eq!(j.kiln.toolchain.as_deref(), Some("rust"));
    assert!(j.kiln.exclusive);
    assert_eq!(j.kiln.artifacts, vec!["target/**".to_owned()]);
    assert!(j.kiln.retry.is_some());
    assert!(j.runs_on.is_none(), "the server's default applies");
    let (wf, _) = kiln("name: x\non:\n  manual: false\n  push: {}\njobs:\n  a: { steps: [{run: x}] }\n");
    assert!(wf.unwrap_or_default().trigger("workflow_dispatch").is_none());
}

#[test]
fn w02_ac05_overlays() {
    let base = "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        os: [linux, macos]\n    steps:\n      - id: test\n        run: make test\n";
    let (wf, d) = gha(base);
    assert!(errors(&d).is_empty(), "{:?}", errors(&d));
    let mut wf = wf.unwrap_or_default();
    let before = def_hash(&wf);
    let ov_src = "jobs:\n  build:\n    runs-on: [linux, x64]\n    retry: { max: 2, on: [infra] }\n    prefer: [{ fact: disk.free, higher: true }]\n    strategy:\n      matrix:\n        where: matrix.os != 'macos'\n    steps:\n      test:\n        retry: { max: 1 }\n      missing:\n        retry: { max: 1 }\n  nojob:\n    retry: { max: 1 }\n";
    let (ov, d) = parse_overlay(ov_src);
    assert!(errors(&d).is_empty(), "{:?}", errors(&d));
    let ov = ov.unwrap_or_default();
    let d = apply_overlay(&mut wf, &ov);
    assert_eq!(d.iter().filter(|x| x.code == "KCI4010").count(), 2, "{d:?}");
    let j = wf.job("build").cloned().unwrap_or_default();
    assert!(j.kiln.retry.is_some() && !j.kiln.prefer.is_empty());
    assert!(matches!(&j.runs_on, Some(ir::RunsOn::Labels(l)) if l.len() == 2));
    assert!(matches!(j.strategy.as_ref().and_then(|s| s.matrix.as_ref()), Some(ir::Matrix::Spec(m)) if m.filter.is_some()));
    assert!(j.steps.first().is_some_and(|s| s.kiln.retry.is_some()));
    assert_ne!(def_hash(&wf), before);
    let (_, d) = parse_overlay("jobs:\n  build:\n    steps: []\n    env: { A: b }\n");
    assert!(d.iter().any(|x| x.code == "KCI1010"), "overlays can't change GitHub keys: {d:?}");
}

#[test]
fn file_kinds_and_precedence() {
    assert_eq!(file_kind(".github/workflows/ci.yml"), Some(FileKind::GhaWorkflow));
    assert_eq!(file_kind(".github/workflows/sub/ci.yml"), None, "not subdirectories");
    assert_eq!(file_kind(".kiln/ci.yaml"), Some(FileKind::KilnWorkflow));
    assert_eq!(file_kind(".kiln/review.yaml"), Some(FileKind::Policy));
    assert_eq!(file_kind(".kiln/overlays/ci.yaml"), Some(FileKind::Overlay { stem: "ci".into() }));
    assert_eq!(file_kind(".github/actions/setup/action.yml"), Some(FileKind::Action));
    assert_eq!(file_kind("README.md"), None);
    assert!(!github_workflows_run(GithubWorkflows::Auto, true).0);
    assert!(github_workflows_run(GithubWorkflows::Auto, false).0);
    assert!(github_workflows_run(GithubWorkflows::On, true).0);
    assert!(!github_workflows_run(GithubWorkflows::Off, false).0);
}

#[test]
fn actions() {
    let (a, d) = parse_action("name: x\ninputs:\n  token:\n    required: 'true'\n    default: ${{ github.token }}\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n    - run: echo\n      shell: bash\nbranding: {icon: x}\nweird: 1\n");
    let a = a.unwrap_or_default();
    assert_eq!(a.runs.steps.len(), 2);
    assert!(a.inputs.first().is_some_and(|i| i.required));
    assert!(codes(&d).contains(&"KCI1020"), "composite run needs shell");
    assert!(d.iter().any(|x| x.code == "KCI1010" && x.severity == Severity::Warning), "unknown keys only warn");
    let (_, d) = parse_action("runs:\n  using: node20\n");
    assert!(codes(&d).contains(&"KCI1014"));
}

#[test]
fn schemas_are_generated() {
    for k in [SchemaKind::Gha, SchemaKind::Kiln, SchemaKind::Overlay, SchemaKind::Action] {
        let s = schema(k);
        assert_eq!(s.get("type").and_then(|t| t.as_str()), Some("object"));
    }
    let g = schema(SchemaKind::Gha);
    let job = g.pointer("/properties/jobs/patternProperties/^[A-Za-z_][A-Za-z0-9_-]*$/properties");
    assert!(job.is_some_and(|p| p.get("runs-on").is_some() && p.get("retry").is_none()), "GHA jobs have no kiln keys");
    let k = schema(SchemaKind::Kiln);
    let job = k.pointer("/properties/jobs/patternProperties/^[A-Za-z_][A-Za-z0-9_-]*$/properties");
    assert!(job.is_some_and(|p| p.get("retry").is_some()));
}

#[test]
fn diagnostics_point_into_block_scalars() {
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - run: |\n          echo one\n          echo ${{ secrets }}\n          echo ${{ bad( }}\n");
    let e = d.iter().find(|x| x.is_error()).map(|x| (x.range.start.line, x.range.start.col));
    assert_eq!(e, Some((9, 23)), "the `(` of `bad(`: {d:?}");
}

#[test]
fn check_reads_a_file_as_its_path_says() {
    let wf = "on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: echo}]\n";
    let c = check(".github/workflows/ci.yml", wf, None);
    assert_eq!((c.kind, c.dialect, c.has_errors()), ("workflow", Dialect::Gha, false));
    assert_eq!(c.outline.as_ref().and_then(|o| o.pointer("/jobs/0/id").and_then(serde_json::Value::as_str)), Some("a"));
    // The same text under .kiln/ is kiln's dialect; a dialect given wins over the path.
    assert_eq!(check(".kiln/ci.yaml", wf, None).dialect, Dialect::Kiln);
    assert_eq!(check("anything.yml", wf, Some(Dialect::Gha)).dialect, Dialect::Gha);
    assert_eq!(check(".kiln/overlays/ci.yaml", "jobs: {}\n", None).kind, "overlay");
    assert_eq!(check(".github/actions/x/action.yml", "name: x\nruns: {using: composite, steps: []}\n", None).kind, "action");
    assert_eq!(check(".kiln/review.yaml", "anything: [", None).kind, "policy");
    assert!(check(".github/workflows/bad.yml", "on: push\njobs: [", None).has_errors());
}

#[test]
fn background_steps_follow_githubs_rules() {
    let job = |steps: &str| format!("on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n{steps}");
    // A wait, wait-all or cancel step always runs.
    let (_, d) = gha(&job("      - id: s\n        run: serve\n        background: true\n      - wait: s\n        if: always()\n"));
    assert!(errors(&d).iter().any(|e| e.contains("KCI1012") && e.contains("always runs")), "{d:?}");
    // cancel names one step.
    let (_, d) = gha(&job("      - id: s\n        run: serve\n        background: true\n      - cancel: [s]\n"));
    assert!(errors(&d).iter().any(|e| e.contains("KCI1011")), "{d:?}");
    let (_, d) = gha(&job("      - id: s\n        run: serve\n        background: true\n      - cancel: s\n"));
    assert!(errors(&d).is_empty(), "{d:?}");
    // A background expression reads what continue-on-error may.
    let (_, d) = gha(&job("      - id: s\n        run: serve\n        background: ${{ jobs.x.result == 'y' }}\n      - wait-all:\n"));
    assert!(errors(&d).iter().any(|e| e.contains("KCI3001")), "{d:?}");
    let (_, d) = gha(&job("      - id: s\n        run: serve\n        background: ${{ matrix.slow == 'yes' }}\n      - wait-all:\n"));
    assert!(errors(&d).is_empty(), "{d:?}");
    // A composite action's steps can't run in the background.
    let (_, d) = parse_action("name: x\nruns:\n  using: composite\n  steps:\n    - run: serve\n      shell: bash\n      background: true\n");
    assert!(d.iter().any(|x| x.code == "KCI1020" && x.message.contains("background")), "{d:?}");
}

/// Found by W02-AC06's mutations: a job that needs itself looped forever.
#[test]
fn needs_of_itself_and_twice_end() {
    let (_, d) = gha("on: push\njobs:\n  build:\n    runs-on: x\n    needs: build\n    steps: [{run: a}]\n");
    assert_eq!(codes(&d).iter().filter(|c| **c == "KCI4002").count(), 1, "{d:?}");
    // The same job twice is one need, not a cycle.
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    runs-on: x\n    needs: [a, a]\n    steps: [{run: b}]\n");
    assert!(errors(&d).is_empty(), "{d:?}");
    let (_, d) = gha("on: push\njobs:\n  a:\n    runs-on: x\n    needs: b\n    steps: [{run: a}]\n  b:\n    runs-on: x\n    needs: [a, b]\n    steps: [{run: b}]\n");
    assert!(errors(&d).iter().any(|e| e.contains("form a cycle")), "{d:?}");
}
