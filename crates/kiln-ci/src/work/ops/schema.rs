//! The operations' JSON Schema (W25 on W24-R29): the serialized form of [`Op`] as serde writes
//! and reads it, for SDKs in other languages to be generated from. Enums are externally tagged
//! (`{"exec": {…}}`, `"merge"`), names in `snake_case`, an absent `Option` is `null`. The tests
//! hold it to the types: an example of every variant validates, and a new variant doesn't compile
//! until it has one.
//!
//! [`Op`]: super::Op

use serde_json::{Value, json};

fn obj(props: &[(&str, Value)], optional: &[&str]) -> Value {
    let properties: serde_json::Map<String, Value> = props.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
    let required: Vec<&str> = props.iter().map(|(k, _)| *k).filter(|k| !optional.contains(k)).collect();
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

fn r(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{name}") })
}

fn nullable(v: Value) -> Value {
    json!({ "oneOf": [v, { "type": "null" }] })
}

fn tagged(tag: &str, v: Value) -> Value {
    obj(&[(tag, v)], &[])
}

/// The schema: `$defs` for every type, and `op` at the root.
pub fn schema() -> Value {
    let s = || json!({ "type": "string" });
    let b = || json!({ "type": "boolean" });
    let u = |max: u64| json!({ "type": "integer", "minimum": 0, "maximum": max });
    let i64_ = || json!({ "type": "integer" });
    let strings = || json!({ "type": "array", "items": { "type": "string" } });
    let map = || json!({ "type": "object", "additionalProperties": { "type": "string" } });
    let defs = json!({
        "platform": obj(&[("os", s()), ("arch", s()), ("variant", s())], &["variant"]),
        "image": obj(&[("reference", s()), ("digest", nullable(s())), ("platform", r("platform"))], &[]),
        "git": obj(&[("url", s()), ("reference", s()), ("subdir", s()), ("checksum", nullable(s())), ("keep_git_dir", b()), ("submodules", b())], &[]),
        "http": obj(&[("url", s()), ("checksum", nullable(s())), ("filename", s())], &[]),
        "local": obj(&[("name", s()), ("include", strings()), ("exclude", strings())], &[]),
        "command": { "oneOf": [tagged("argv", strings()), tagged("shell", obj(&[("shell", strings()), ("script", s())], &[]))] },
        "network": { "enum": ["default", "none", "host"] },
        "security": { "enum": ["sandbox", "insecure"] },
        "sharing": { "enum": ["shared", "private", "locked"] },
        "mount_kind": { "oneOf": [
            tagged("bind", obj(&[("source", s()), ("readonly", b())], &[])),
            tagged("cache", obj(&[("id", s()), ("sharing", r("sharing")), ("source", s()), ("readonly", b()), ("mode", nullable(u(u64::from(u32::MAX)))), ("uid", nullable(u(u64::from(u32::MAX)))), ("gid", nullable(u(u64::from(u32::MAX))))], &[])),
            tagged("tmpfs", obj(&[("size", nullable(u(u64::MAX)))], &[])),
            tagged("secret", obj(&[("id", s()), ("env", nullable(s())), ("required", b()), ("mode", nullable(u(u64::from(u32::MAX)))), ("uid", nullable(u(u64::from(u32::MAX)))), ("gid", nullable(u(u64::from(u32::MAX))))], &[])),
            tagged("ssh", obj(&[("id", s()), ("required", b()), ("mode", nullable(u(u64::from(u32::MAX)))), ("uid", nullable(u(u64::from(u32::MAX)))), ("gid", nullable(u(u64::from(u32::MAX))))], &[])),
        ] },
        "mount": obj(&[("target", s()), ("kind", r("mount_kind"))], &[]),
        "exec": obj(&[("command", r("command")), ("env", map()), ("cwd", s()), ("user", s()), ("mounts", json!({ "type": "array", "items": r("mount") })), ("network", r("network")), ("security", r("security")), ("platform", r("platform")), ("proxy", map())], &[]),
        "copy_source": obj(&[("path", s()), ("follow_symlinks", b()), ("contents_only", b()), ("wildcard", b()), ("unpack", b()), ("include", strings()), ("required", strings())], &[]),
        "copy": obj(&[("dest", s()), ("sources", json!({ "type": "array", "items": r("copy_source") })), ("chown", nullable(s())), ("chmod", nullable(s())), ("exclude", strings())], &[]),
        "mkdir": obj(&[("path", s()), ("mode", u(u64::from(u32::MAX))), ("parents", b()), ("owner", nullable(s())), ("created", nullable(i64_()))], &[]),
        "mkfile": obj(&[("path", s()), ("mode", u(u64::from(u32::MAX))), ("data", s()), ("owner", nullable(s()))], &[]),
        "remove": obj(&[("path", s()), ("allow_not_found", b()), ("wildcard", b())], &[]),
        "healthcheck": obj(&[("test", strings()), ("interval", u(u64::MAX)), ("timeout", u(u64::MAX)), ("start_period", u(u64::MAX)), ("start_interval", u(u64::MAX)), ("retries", u(u64::from(u32::MAX)))], &[]),
        "config": { "oneOf": [
            tagged("env", map()), tagged("label", map()), tagged("workdir", s()), tagged("user", s()),
            tagged("entrypoint", obj(&[("args", strings()), ("keep_cmd", b())], &[])),
            tagged("cmd", strings()), tagged("expose", strings()), tagged("volume", strings()), tagged("stop_signal", s()),
            tagged("healthcheck", r("healthcheck")), tagged("shell", strings()), tagged("maintainer", s()), tagged("onbuild", s()),
        ] },
        "export": obj(&[("platform", r("platform")), ("epoch", nullable(i64_()))], &[]),
        "port": obj(&[("port", u(u64::from(u16::MAX))), ("protocol", json!({ "enum": ["tcp", "udp"] }))], &[]),
        "service": obj(&[("command", r("command")), ("env", map()), ("cwd", s()), ("user", s()), ("ports", json!({ "type": "array", "items": r("port") })), ("platform", r("platform"))], &[]),
        "op": { "oneOf": [
            tagged("image", r("image")), tagged("git", r("git")), tagged("http", r("http")), tagged("local", r("local")),
            tagged("exec", r("exec")), tagged("copy", r("copy")), tagged("mkdir", r("mkdir")), tagged("mkfile", r("mkfile")),
            tagged("remove", r("remove")), { "const": "merge" }, tagged("config", r("config")), tagged("export", r("export")),
            tagged("service", r("service")),
        ] },
    });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://kiln.dev/schemas/work-ops.json",
        "title": "kiln work::ops operation",
        "$defs": defs,
        "$ref": "#/$defs/op",
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::super::*;
    use super::*;

    /// Whether `v` holds to `schema` (the subset [`schema`] uses), resolving `$ref`s in `root`.
    fn holds(root: &Value, schema: &Value, v: &Value) -> bool {
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            return r.strip_prefix("#/$defs/").and_then(|n| root.pointer(&format!("/$defs/{n}"))).is_some_and(|d| holds(root, d, v));
        }
        if let Some(all) = schema.get("oneOf").and_then(Value::as_array) {
            return all.iter().filter(|s| holds(root, s, v)).count() == 1;
        }
        if let Some(c) = schema.get("const") {
            return c == v;
        }
        if let Some(e) = schema.get("enum").and_then(Value::as_array) {
            return e.contains(v);
        }
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => v.is_string(),
            Some("boolean") => v.is_boolean(),
            Some("null") => v.is_null(),
            Some("integer") => {
                let (min, max) = (schema.get("minimum").and_then(Value::as_i64), schema.get("maximum").and_then(Value::as_u64));
                match (v.as_u64(), v.as_i64()) {
                    (Some(n), _) => max.is_none_or(|m| n <= m),
                    (None, Some(n)) => min.is_none_or(|m| n >= m),
                    _ => false,
                }
            }
            Some("array") => v.as_array().is_some_and(|a| schema.get("items").is_none_or(|it| a.iter().all(|x| holds(root, it, x)))),
            Some("object") => {
                let Some(o) = v.as_object() else { return false };
                let props = schema.get("properties").and_then(Value::as_object);
                let required = schema.get("required").and_then(Value::as_array).cloned().unwrap_or_default();
                let extra = schema.get("additionalProperties");
                required.iter().filter_map(Value::as_str).all(|k| o.contains_key(k))
                    && o.iter().all(|(k, x)| match props.and_then(|p| p.get(k)) {
                        Some(s) => holds(root, s, x),
                        None => match extra {
                            Some(Value::Bool(false)) => false,
                            Some(s) if s.is_object() => holds(root, s, x),
                            _ => true,
                        },
                    })
            }
            _ => false,
        }
    }

    fn platform() -> Platform {
        Platform { os: "linux".into(), arch: "amd64".into(), variant: String::new() }
    }

    fn exec(mounts: Vec<Mount>, command: Command) -> Op {
        Op::Exec(Exec { command, env: BTreeMap::from([("A".into(), "1".into())]), cwd: "/".into(), user: String::new(), mounts, network: Network::None, security: Security::Sandbox, platform: platform(), proxy: BTreeMap::new() })
    }

    /// One example of every operation, every command form, every mount and every config edit.
    fn examples() -> Vec<Op> {
        let mounts = vec![
            Mount { target: "/src".into(), kind: MountKind::Bind { source: "/".into(), readonly: true } },
            Mount { target: "/cache".into(), kind: MountKind::Cache { id: "cargo".into(), sharing: Sharing::Locked, source: String::new(), readonly: false, mode: Some(0o755), uid: None, gid: None } },
            Mount { target: "/tmp".into(), kind: MountKind::Tmpfs { size: Some(1 << 20) } },
            Mount { target: "/run/secrets/t".into(), kind: MountKind::Secret { id: "t".into(), env: None, required: true, mode: None, uid: None, gid: None } },
            Mount { target: "/ssh".into(), kind: MountKind::Ssh { id: "default".into(), required: false, mode: None, uid: Some(0), gid: Some(0) } },
        ];
        let edits = vec![
            ConfigEdit::Env(BTreeMap::from([("PATH".into(), "/bin".into())])),
            ConfigEdit::Label(BTreeMap::new()),
            ConfigEdit::Workdir("/app".into()),
            ConfigEdit::User("app".into()),
            ConfigEdit::Entrypoint { args: vec!["/app".into()], keep_cmd: false },
            ConfigEdit::Cmd(vec!["serve".into()]),
            ConfigEdit::Expose(BTreeSet::from(["8080/tcp".into()])),
            ConfigEdit::Volume(BTreeSet::from(["/data".into()])),
            ConfigEdit::StopSignal("SIGTERM".into()),
            ConfigEdit::Healthcheck(Healthcheck { test: vec!["CMD".into(), "true".into()], interval: 30, timeout: 5, start_period: 0, start_interval: 0, retries: 3 }),
            ConfigEdit::Shell(vec!["/bin/sh".into(), "-c".into()]),
            ConfigEdit::Maintainer("kiln".into()),
            ConfigEdit::Onbuild("RUN true".into()),
        ];
        let mut out = vec![
            Op::Image(Image { reference: "alpine:3".into(), digest: Some("sha256:00".into()), platform: platform() }),
            Op::Git(Git { url: "https://example.com/r.git".into(), reference: "main".into(), subdir: String::new(), checksum: None, keep_git_dir: false, submodules: true }),
            Op::Http(Http { url: "https://example.com/f".into(), checksum: None, filename: "f".into() }),
            Op::Local(Local { name: "context".into(), include: vec!["src".into()], exclude: Vec::new() }),
            exec(mounts, Command::Argv(vec!["make".into()])),
            exec(Vec::new(), Command::Shell { shell: vec!["/bin/sh".into(), "-c".into()], script: "make".into() }),
            Op::Copy(CopyFiles { dest: "/app".into(), sources: vec![CopySource { path: "/src".into(), follow_symlinks: true, contents_only: false, wildcard: false, unpack: false, include: Vec::new(), required: Vec::new() }], chown: None, chmod: Some("755".into()), exclude: Vec::new() }),
            Op::Mkdir(Mkdir { path: "/app".into(), mode: 0o755, parents: true, owner: None, created: Some(0) }),
            Op::Mkfile(Mkfile { path: "/app/f".into(), mode: 0o644, data: "x".into(), owner: Some("0:0".into()) }),
            Op::Remove(Remove { path: "/tmp/x".into(), allow_not_found: true, wildcard: false }),
            Op::Merge,
            Op::Export(Export { platform: platform(), epoch: None }),
            Op::Service(Service { command: Command::Argv(vec!["postgres".into()]), env: BTreeMap::new(), cwd: "/".into(), user: "postgres".into(), ports: vec![Port { port: 5432, protocol: Protocol::Tcp }, Port { port: 53, protocol: Protocol::Udp }], platform: platform() }),
        ];
        out.extend(edits.into_iter().map(Op::Config));
        out
    }

    /// W25 on W24-R29: every operation, command form, mount and config edit, as serde writes it,
    /// holds to the schema; what the types refuse, the schema refuses.
    #[test]
    fn w25_every_operation_holds_to_the_schema() -> Result<(), String> {
        let root = schema();
        let op = root.pointer("/$defs/op").cloned().ok_or("no op")?;
        let all = examples();
        // A new variant doesn't compile here until it has an example above, and the schema with it.
        for o in &all {
            match o {
                Op::Image(_) | Op::Git(_) | Op::Http(_) | Op::Local(_) | Op::Exec(_) | Op::Copy(_) | Op::Mkdir(_) | Op::Mkfile(_) | Op::Remove(_) | Op::Merge | Op::Config(_) | Op::Export(_) | Op::Service(_) => {}
            }
            if let Op::Exec(e) = o {
                for m in &e.mounts {
                    match m.kind {
                        MountKind::Bind { .. } | MountKind::Cache { .. } | MountKind::Tmpfs { .. } | MountKind::Secret { .. } | MountKind::Ssh { .. } => {}
                    }
                }
            }
            if let Op::Config(c) = o {
                match c {
                    ConfigEdit::Env(_) | ConfigEdit::Label(_) | ConfigEdit::Workdir(_) | ConfigEdit::User(_) | ConfigEdit::Entrypoint { .. } | ConfigEdit::Cmd(_) | ConfigEdit::Expose(_) | ConfigEdit::Volume(_) | ConfigEdit::StopSignal(_) | ConfigEdit::Healthcheck(_) | ConfigEdit::Shell(_) | ConfigEdit::Maintainer(_) | ConfigEdit::Onbuild(_) => {}
                }
            }
            let v = serde_json::to_value(o).map_err(|e| e.to_string())?;
            if !holds(&root, &op, &v) {
                return Err(format!("{} doesn't hold to the schema: {v}", o.kind()));
            }
            let back: Op = serde_json::from_value(v).map_err(|e| e.to_string())?;
            if &back != o {
                return Err(format!("{} doesn't come back as it went", o.kind()));
            }
        }
        let kinds: BTreeSet<&str> = all.iter().map(Op::kind).collect();
        if kinds.len() != 13 {
            return Err(format!("every kind has an example: {kinds:?}"));
        }
        for bad in [json!({ "exec": { "command": 3 } }), json!({ "image": { "reference": "a" } }), json!("copy"), json!({ "mkdir": { "path": "/", "mode": -1, "parents": true, "owner": null, "created": null } })] {
            if holds(&root, &op, &bad) || serde_json::from_value::<Op>(bad.clone()).is_ok() {
                return Err(format!("refused by both: {bad}"));
            }
        }
        Ok(())
    }

    /// The schema as `docs/work-ops.schema.json` publishes it, for SDK generators: the code's, as
    /// JSON. `KILN_BLESS=1` writes it.
    #[test]
    fn w25_the_published_schema_is_the_codes() -> Result<(), String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/work-ops.schema.json");
        let want = schema();
        if std::env::var_os("KILN_BLESS").is_some() {
            let text = serde_json::to_string_pretty(&want).map_err(|e| e.to_string())?;
            std::fs::write(path, text + "\n").map_err(|e| e.to_string())?;
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let got: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if got != want {
            return Err(format!("{path} isn't the code's schema: run this test with KILN_BLESS=1"));
        }
        Ok(())
    }
}
