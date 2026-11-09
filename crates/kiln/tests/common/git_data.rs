//! W29-R25: GitHub's Git Data API over a fixture's real objects and refs. A saved draft head
//! must be fetchable on its next save; invented blob/tree/commit ids cannot hold that contract.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use serde_json::{Value, json};

pub struct GitData { home: PathBuf, repo: PathBuf, index: AtomicU64 }

impl GitData {
    pub fn new(home: PathBuf, repo: PathBuf) -> Self { Self { home, repo, index: AtomicU64::new(0) } }

    fn git(&self, args: &[&str], input: &[u8], env: &[(&str, &str)]) -> anyhow::Result<String> {
        let mut command = Command::new("git");
        command.arg("--git-dir").arg(&self.repo).args(args).env("HOME", &self.home).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Fixture").env("GIT_AUTHOR_EMAIL", "fixture@example.test").env("GIT_COMMITTER_NAME", "Fixture").env("GIT_COMMITTER_EMAIL", "fixture@example.test")
            .env_remove("GIT_ASKPASS").env_remove("SSH_ASKPASS").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for (name, value) in env { command.env(name, value); }
        let mut child = command.spawn()?;
        child.stdin.take().ok_or_else(|| anyhow::anyhow!("git stdin missing"))?.write_all(input)?;
        let result = child.wait_with_output()?;
        anyhow::ensure!(result.status.success(), "fixture git {args:?}: {}", String::from_utf8_lossy(&result.stderr));
        Ok(String::from_utf8(result.stdout)?.trim().to_owned())
    }

    pub fn reply(&self, method: &str, path: &str, body: &Value) -> Option<anyhow::Result<(u16, Value)>> {
        let path = path.strip_prefix("/repos/acme/app/git/")?;
        Some(self.apply(method, path, body))
    }

    fn apply(&self, method: &str, path: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
        match (method, path) {
            ("GET", path) if path.starts_with("commits/") => {
                let sha = path.trim_start_matches("commits/");
                let tree = self.git(&["rev-parse", "--verify", &format!("{sha}^{{tree}}")], &[], &[])?;
                Ok((200, json!({ "sha": sha, "tree": {"sha": tree} })))
            }
            ("POST", "blobs") => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(body.get("content").and_then(Value::as_str).unwrap_or_default())?;
                let sha = self.git(&["hash-object", "-w", "--stdin"], &bytes, &[])?;
                Ok((201, json!({"sha": sha})))
            }
            ("POST", "trees") => {
                let index = self.home.join(format!("git-data-index-{}", self.index.fetch_add(1, Ordering::SeqCst)));
                struct Remove(PathBuf);
                impl Drop for Remove { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
                let _remove = Remove(index.clone());
                let index = index.to_string_lossy();
                let env = [("GIT_INDEX_FILE", index.as_ref())];
                match body.get("base_tree").and_then(Value::as_str) {
                    Some(tree) => { self.git(&["read-tree", tree], &[], &env)?; }
                    None => { self.git(&["read-tree", "--empty"], &[], &env)?; }
                }
                for entry in body.get("tree").and_then(Value::as_array).into_iter().flatten() {
                    let path = entry.get("path").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("tree path missing"))?;
                    match entry.get("sha").and_then(Value::as_str) {
                        Some(sha) => {
                            let mode = entry.get("mode").and_then(Value::as_str).unwrap_or("100644");
                            self.git(&["update-index", "--add", "--cacheinfo", &format!("{mode},{sha},{path}")], &[], &env)?;
                        }
                        None => { self.git(&["update-index", "--force-remove", "--", path], &[], &env)?; }
                    }
                }
                Ok((201, json!({"sha": self.git(&["write-tree"], &[], &env)?})))
            }
            ("POST", "commits") => {
                let tree = body.get("tree").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("commit tree missing"))?;
                let mut args = vec!["commit-tree", tree];
                for parent in body.get("parents").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) { args.extend(["-p", parent]); }
                args.extend(["-F", "-"]);
                let name = body.pointer("/author/name").and_then(Value::as_str).unwrap_or("Fixture");
                let email = body.pointer("/author/email").and_then(Value::as_str).unwrap_or("fixture@example.test");
                let env = [("GIT_AUTHOR_NAME", name), ("GIT_AUTHOR_EMAIL", email), ("GIT_COMMITTER_NAME", name), ("GIT_COMMITTER_EMAIL", email)];
                let message = body.get("message").and_then(Value::as_str).unwrap_or_default();
                Ok((201, json!({"sha": self.git(&args, message.as_bytes(), &env)?})))
            }
            ("POST", "refs") => {
                let reference = body.get("ref").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("ref missing"))?;
                let sha = body.get("sha").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("ref sha missing"))?;
                if self.git(&["show-ref", "--verify", reference], &[], &[]).is_ok() { return Ok((422, json!({"message": "ref already exists"}))); }
                self.git(&["update-ref", reference, sha, "0000000000000000000000000000000000000000"], &[], &[])?;
                Ok((201, json!({"ref": reference})))
            }
            ("PATCH", path) if path.starts_with("refs/heads/") => {
                let sha = body.get("sha").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("ref sha missing"))?;
                let old = self.git(&["rev-parse", "--verify", path], &[], &[])?;
                if self.git(&["merge-base", "--is-ancestor", &old, sha], &[], &[]).is_err() { return Ok((422, json!({"message": "not a fast-forward"}))); }
                self.git(&["update-ref", path, sha, &old], &[], &[])?;
                Ok((200, json!({"ref": path})))
            }
            ("DELETE", path) if path.starts_with("refs/heads/") => {
                self.git(&["update-ref", "-d", path], &[], &[])?;
                Ok((204, Value::Null))
            }
            _ => Ok((404, json!({"message": "unknown fixture Git Data route"}))),
        }
    }
}
