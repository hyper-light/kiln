//! Pipelines for repositories that have not written `.kiln/*.yaml` yet: the project type is read
//! from the files at the commit's root, and a conservative build + test pipeline is synthesised.
//! As soon as a repository adds its own `.kiln/` directory, that takes precedence (GitOps: the
//! repository owns its pipeline).

/// Root-level file names at a commit → a pipeline YAML, or `None` if nothing is buildable.
pub fn pipeline_for(root_files: &[String]) -> Option<String> {
    let has = |f: &str| root_files.iter().any(|x| x == f);
    let mut jobs = Vec::new();
    if has("Cargo.toml") {
        jobs.push(RUST);
    }
    if has("go.mod") || has("go.work") {
        jobs.push(GO);
    }
    if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        jobs.push(PYTHON);
    }
    if has("package.json") {
        jobs.push(NODE);
    }
    if has("rebar.config") {
        jobs.push(ERLANG);
    }
    if jobs.is_empty() {
        return None;
    }
    Some(format!("{HEADER}{}", jobs.concat()))
}

const HEADER: &str = r#"name: auto
on:
  push: { branches: [main, master] }
  pull_request: {}
  manual: true
concurrency:
  group: "auto-${{ branch }}"
  cancel-in-progress: true
jobs:
"#;

const RUST: &str = r#"  rust:
    name: rust build + test
    runs-on: [k8s]
    toolchain: rust
    resources: { cpu: "4", memory: 8Gi }
    timeout-minutes: 90
    # CI-friendly profile: line tables instead of full debuginfo cuts linker memory several-fold
    # (large workspaces otherwise get the linker OOM-killed), no incremental cache to bloat.
    env:
      CARGO_INCREMENTAL: "0"
      CARGO_PROFILE_DEV_DEBUG: line-tables-only
      CARGO_PROFILE_TEST_DEBUG: line-tables-only
      CARGO_BUILD_JOBS: "4"
    steps:
      - name: system deps
        run: |
          missing=""
          for b in cmake clang protoc pkg-config; do command -v $b >/dev/null || missing=1; done
          if [ -n "$missing" ] && command -v apt-get >/dev/null; then
            apt-get update -qq && apt-get install -y -qq cmake clang libclang-dev protobuf-compiler pkg-config libssl-dev >/dev/null
          fi
          rustup show active-toolchain || rustup show
      - name: build
        run: cargo build --workspace --all-targets --locked
      - name: test
        run: cargo test --workspace --locked
"#;

const GO: &str = r#"  go:
    name: go build + test
    runs-on: [k8s]
    toolchain: go
    resources: { cpu: "2", memory: 4Gi }
    timeout-minutes: 45
    steps:
      - run: go version && go build ./...
      - run: go vet ./...
      - run: go test ./...
"#;

const PYTHON: &str = r#"  python:
    name: python install + test
    runs-on: [k8s]
    toolchain: python
    resources: { cpu: "2", memory: 6Gi }
    timeout-minutes: 45
    steps:
      - name: install
        run: |
          install() {
            if [ -f uv.lock ]; then uv sync --all-extras --all-groups --frozen || return 1
            elif [ -f pyproject.toml ]; then uv sync --all-extras --all-groups || { uv venv -q --allow-existing && uv pip install -e .; } || return 1
            elif [ -f setup.py ]; then uv venv -q --allow-existing && uv pip install -e . || return 1
            else uv venv -q --allow-existing && uv pip install -r requirements.txt || return 1; fi
            for f in requirements.txt requirements-dev.txt requirements-test.txt dev-requirements.txt test-requirements.txt; do
              if [ -f "$f" ]; then uv pip install -q -r "$f" || return 1; fi
            done
          }
          # The project's .python-version wins; otherwise try the image default, then fall back to
          # an interpreter that pinned dependencies are most likely to have wheels for.
          if [ -f .python-version ]; then install; exit $?; fi
          install && exit 0
          for v in 3.13 3.12; do
            echo "[kiln] install failed; retrying with Python $v"
            rm -rf .venv
            UV_PYTHON=$v install && echo "$v" > .python-version && exit 0
          done
          exit 1
      - name: test
        run: |
          # No test suite: a clean install is the check (a repo-wide compile sweep trips on
          # vendored fixtures that are deliberately invalid Python).
          # Honour a configured testpaths; otherwise target tests/ (or test/) so stray scripts
          # elsewhere in the repo (examples, notebooks) are not collected.
          if grep -qs "testpaths" pyproject.toml pytest.ini setup.cfg tox.ini; then uv run --with pytest pytest -q
          elif [ -d tests ]; then uv run --with pytest pytest -q tests
          elif [ -d test ]; then uv run --with pytest pytest -q test
          else echo "no tests/ directory; install verified"; fi
"#;

const NODE: &str = r#"  node:
    name: node lint + test + build
    runs-on: [k8s]
    toolchain: node
    resources: { cpu: "2", memory: 4Gi }
    timeout-minutes: 45
    steps:
      - run: if [ -f package-lock.json ]; then npm ci --no-audit --no-fund; else npm install --no-audit --no-fund; fi
      - run: npm run lint --if-present
      - run: npm run typecheck --if-present
      - run: npm test --if-present
      - run: npm run build --if-present
"#;

const ERLANG: &str = r#"  erlang:
    name: erlang compile + test
    runs-on: [k8s]
    toolchain: erlang
    timeout-minutes: 45
    steps:
      - run: rebar3 compile
      - run: rebar3 eunit
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_parses() -> anyhow::Result<()> {
        assert!(pipeline_for(&["README.md".into(), "LICENSE".into()]).is_none());
        let y = pipeline_for(&["Cargo.toml".into(), "package.json".into()])
            .ok_or_else(|| anyhow::anyhow!("expected a pipeline"))?;
        let p = crate::pipeline::parse(&y)?;
        assert_eq!(p.jobs.len(), 2);
        for files in [vec!["go.mod"], vec!["pyproject.toml"], vec!["rebar.config"]] {
            let files: Vec<String> = files.into_iter().map(str::to_owned).collect();
            let y = pipeline_for(&files).ok_or_else(|| anyhow::anyhow!("expected a pipeline"))?;
            crate::pipeline::parse(&y)?;
        }
        Ok(())
    }
}
