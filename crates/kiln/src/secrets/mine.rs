//! A person's own secrets (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, decision 6; P4.7): the
//! `user` scope of CI's store (`crate::ci::secrets`, W09), sealed as every scope's values are.
//!
//! - **Theirs alone.** Each person sets, lists and deletes their own (`/api/me/secrets`, the
//!   operations `secrets.mine.*`: kiln's MCP tools `secrets_mine_*`, code mode's
//!   `kiln.secrets.mine.*`), held to the person they are (`harness::lineage::person_of`). No one
//!   reaches another's, by name or by value: not the server's owner, whose own are `owner`'s, nor
//!   `secrets.manage` (`/api/ci/secrets` refuses the scope). The owner's break-glass is the admin
//!   token, which signs in as the owner and so reaches the owner's own alone; what reaches the rest
//!   is the server's disk (its database and master key), as for every scope.
//! - **Never read back.** A list names each with its size and when it was set and last used; no
//!   answer, log or audit row carries a value. An agent lists its person's names (to name them in an
//!   Agentfile) and sets none: a value it set would have passed through its conversation.
//! - **Given to their builds alone.** One of a person's own Agentfiles names each secret a build of
//!   it is given as `LABEL kiln.secret.<ID>=user:<NAME>` ([`references`]): `<ID>` is the build
//!   secret Shards reads (`GIT_AUTH_TOKEN`, `GIT_AUTH_TOKEN.<host>`, `HTTP_AUTH_HEADER_<host>`,
//!   `AWS_ACCESS_KEY_ID`, … for an `AGENT`/`HARNESS` source; any `RUN --mount=type=secret,id=<ID>`),
//!   `<NAME>` one of theirs. kiln's server reads them as the session of that person starts
//!   ([`for_build`]), refusing a name they hold none of before anything is placed, and its runner
//!   gives them to that build alone (`sandbox::image`), never keeping them in the session. The text
//!   carries names only: a reference that isn't `user:<NAME>` is refused when the text is kept
//!   (`agentfiles::check`), its words never repeating what was written. Its build is given the text
//!   without its references ([`built_text`]), so no name of the person's reaches the image (its
//!   config's labels, its build's definition and source map, its provenance): who can inspect an
//!   image learns nothing of whose secrets built it.
//! - **SSH keys too** (kiln plan 01a1dd2e… P4.6): `LABEL kiln.ssh.<ID>=user:<NAME>` names one of
//!   theirs that holds a private key, which its build is given as SSH agent `<ID>` (Shards' `--ssh
//!   <ID>=<key file>`; an `AGENT` or `HARNESS` Git source over SSH asks for `default`), read, refused
//!   and kept out of its image as a build secret's reference is ([`Given`]).

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ci::secrets as store;
use crate::ops::{Cx, Fail, Input, Op, Out, path};

/// What an Agentfile's label naming a build secret starts with: `LABEL kiln.secret.<ID>=user:<NAME>`.
pub const LABEL: &str = "kiln.secret.";

/// What a reference's value starts with: the scope it's read from, a person's own.
pub const USER: &str = "user:";

/// What an Agentfile's label naming an SSH key starts with: `LABEL kiln.ssh.<ID>=user:<NAME>` (P4.6).
pub const SSH: &str = "kiln.ssh.";

/// How a build is given one of its person's secrets (kiln plan 01a1dd2e… P4.6): as a build secret by its id, or as an
/// SSH agent by its id holding the private key it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileGiven"))]
#[serde(rename_all = "lowercase")]
pub enum Given {
    /// `LABEL kiln.secret.<ID>=…`, Shards' `--secret id=<ID>`: a token, a header, a key's half.
    #[default]
    Secret,
    /// `LABEL kiln.ssh.<ID>=…`, Shards' `--ssh <ID>=<key file>`.
    Ssh,
}

impl Given {
    /// What an Agentfile's label naming one starts with.
    pub fn label(self) -> &'static str {
        match self {
            Given::Secret => LABEL,
            Given::Ssh => SSH,
        }
    }

    /// The one of these an Agentfile's label `key` names, and the id past its prefix.
    fn of(key: &str) -> Option<(Given, &str)> {
        [Given::Secret, Given::Ssh].into_iter().find_map(|g| key.strip_prefix(g.label()).map(|id| (g, id)))
    }
}

/// One secret a build of a person's own Agentfile is given: Shards' id for it, and its value, read
/// from that person's own by kiln's server as the session started. Its value is never shown:
/// `Debug` names its id alone.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSecret {
    pub id: String,
    pub value: String,
    /// As a build secret, or as an SSH agent's key (P4.6); one from a server before it says none: a build secret.
    #[serde(default)]
    pub given: Given,
}

impl std::fmt::Debug for BuildSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildSecret").field("id", &self.id).field("given", &self.given).finish_non_exhaustive()
    }
}

/// One `LABEL kiln.secret.<ID>=user:<NAME>` of an Agentfile: the build secret, and whose own it's read
/// from by its name; or one `LABEL kiln.ssh.<ID>=user:<NAME>`, an SSH agent's key (`given`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub id: String,
    pub name: String,
    pub given: Given,
}

/// Why a `kiln.secret.` or `kiln.ssh.` label isn't a reference, and where: its words never repeat its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Misnamed {
    pub message: String,
    /// Its instruction's lines (Shards' `Location`).
    pub location: Vec<(usize, usize)>,
}

/// Whether `id` is a build secret's id Shards' `--secret id=` takes as one field: letters, digits,
/// `_`, `.` and `-` (a host's name among them), none of the CSV's `,`, `=` or quotes.
fn build_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `s` without one pair of quotes around it, as a label's word is written either way.
fn unquoted(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// The build secrets and SSH keys `text` names (`LABEL kiln.secret.<ID>=user:<NAME>`, `LABEL
/// kiln.ssh.<ID>=user:<NAME>`, in any stage), each once by its id, and each such label that isn't one, with
/// why. Read with Shards' own parser; a text it can't read names none (its check says why).
pub fn references(text: &str) -> (Vec<Reference>, Vec<Misnamed>) {
    use shards_agentfile::instructions::{self, Kind};
    use shards_agentfile::{lint, parser};
    let (mut refs, mut bad): (Vec<Reference>, Vec<Misnamed>) = (Vec::new(), Vec::new());
    let Ok(parsed) = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile) else { return (refs, bad) };
    let Ok(read) = instructions::parse(&parsed, &lint::Linter::default()) else { return (refs, bad) };
    for c in read.stages.iter().flat_map(|s| &s.commands) {
        let Kind::Label(pairs) = &c.kind else { continue };
        let location = c.location.clone();
        for kv in pairs {
            let key = String::from_utf8_lossy(&kv.key);
            let Some((given, id)) = Given::of(unquoted(&key)).map(|(g, id)| (g, id.to_owned())) else { continue };
            let label = given.label();
            let value = String::from_utf8_lossy(&kv.value);
            let name = unquoted(&value).strip_prefix(USER).map(str::to_owned);
            let said = match name.as_deref().map(kiln_ci::secrets::check_name) {
                _ if !build_id(&id) => Some(format!("LABEL {label}{id}: a build secret's id is letters, digits, `_`, `.` and `-` (`GIT_AUTH_TOKEN`, `GIT_AUTH_TOKEN.github.com`)")),
                Some(Ok(_)) if refs.iter().any(|r| r.id == id && r.given == given) => Some(format!("LABEL {label}{id} names its secret twice: once is all a build takes")),
                Some(Ok(name)) => {
                    refs.push(Reference { id, name, given });
                    None
                }
                Some(Err(why)) => Some(format!("LABEL {label}{id}: user:<NAME> names one of your secrets, and {why}")),
                None => Some(format!("LABEL {label}{id} names its secret as user:<NAME>, one of your own (set at /api/me/secrets): a secret's value never goes in an Agentfile")),
            };
            if let Some(message) = said {
                bad.push(Misnamed { message, location: location.clone() });
            }
        }
    }
    (refs, bad)
}

/// `text` as its build is given it: every `kiln.secret.` and `kiln.ssh.` label pair taken out (an instruction left with
/// none, a blank line), its other pairs as written and every other line as it was, so that Shards'
/// words name the lines the person wrote. Its references stay kiln's: no name of theirs reaches the
/// image's config, its build's definition and source map, or its provenance. A text Shards can't read
/// is given as it is (its build says why, in Shards' words, as its check did).
pub fn built_text(text: &str) -> String {
    use shards_agentfile::instructions::{self, Kind};
    use shards_agentfile::{lint, parser};
    let Ok(parsed) = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile) else { return text.to_owned() };
    let Ok(read) = instructions::parse(&parsed, &lint::Linter::default()) else { return text.to_owned() };
    // Each line, its ending kept; a label's lines, from its first, become what's left of it.
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    for c in read.stages.iter().flat_map(|s| &s.commands) {
        let Kind::Label(pairs) = &c.kind else { continue };
        let named = |k: &[u8]| Given::of(unquoted(&String::from_utf8_lossy(k))).is_some();
        if !pairs.iter().any(|kv| named(&kv.key)) {
            continue;
        }
        let (Some(first), Some(last)) = (c.location.iter().map(|r| r.0).min(), c.location.iter().map(|r| r.1.max(r.0)).max()) else { continue };
        let kept: Vec<String> = pairs
            .iter()
            .filter(|kv| !named(&kv.key))
            .map(|kv| {
                let (k, v) = (String::from_utf8_lossy(&kv.key), String::from_utf8_lossy(&kv.value));
                if kv.no_delim { format!("{k} {v}") } else { format!("{k}={v}") }
            })
            .collect();
        for at in first..=last {
            let Some(line) = at.checked_sub(1).and_then(|i| lines.get_mut(i)) else { continue };
            let ending = if line.ends_with("\r\n") { "\r\n" } else if line.ends_with('\n') { "\n" } else { "" };
            let instead = if at == first && !kept.is_empty() { format!("{} {}", String::from_utf8_lossy(&c.name), kept.join(" ")) } else { String::new() };
            *line = format!("{instead}{ending}");
        }
    }
    lines.concat()
}

/// What a build of `person`'s own Agentfile `text` is given (decision 6): each secret it names, read
/// from `person`'s own; else why not, naming what's missing and never a value. A text naming none is
/// given none.
pub async fn for_build(hub: &crate::hub::Hub, person: &str, text: &str) -> Result<Vec<BuildSecret>, String> {
    let (refs, bad) = references(text);
    if let Some(m) = bad.first() {
        return Err(m.message.clone());
    }
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
    let (held, missing) = store::for_build(hub, person, &names).await.map_err(|e| format!("your secrets: {e:#}"))?;
    if !missing.is_empty() {
        let named: Vec<String> = refs.iter().filter(|r| missing.contains(&r.name)).map(|r| format!("{}{} names {USER}{}", r.given.label(), r.id, r.name)).collect();
        return Err(format!("its Agentfile's {}, which isn't one of your secrets: set it at /api/me/secrets (secrets.mine.set)", named.join(", ")));
    }
    Ok(refs.into_iter().filter_map(|r| held.get(&r.name).map(|v| BuildSecret { id: r.id, value: v.clone(), given: r.given })).collect())
}

/// Whose own the caller reaches: the person they are, or the person an agent acts for.
fn person(cx: &Cx) -> Result<String, Fail> {
    crate::harness::lineage::person_of(&cx.caller).ok_or_else(|| (StatusCode::FORBIDDEN, format!("{} acts for no person here, and a person's own secrets are theirs", cx.caller.name)))
}

/// A person's own, never an agent's word for them: a value an agent set would have been in its conversation.
fn by_a_person(cx: &Cx) -> Result<String, Fail> {
    if cx.caller.agent.is_some() {
        return Err((StatusCode::FORBIDDEN, "a person sets and deletes their own secrets themselves: an agent only lists their names".into()));
    }
    person(cx)
}

async fn list(cx: Cx, _v: Value) -> Out {
    let scope = store::user_scope(&person(&cx)?);
    let secrets = store::list(&cx.hub, &scope).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(json!({ "secrets": secrets }))
}

async fn set(cx: Cx, v: Value) -> Out {
    let scope = store::user_scope(&by_a_person(&cx)?);
    let name = crate::ops::text(&v, "name")?;
    let value = v.get("value").and_then(Value::as_str).ok_or_else(|| (StatusCode::BAD_REQUEST, "`value` is required: the secret's value, kept sealed and never shown again".to_owned()))?;
    store::set(&cx.hub, &scope, &name, value, &cx.caller.name).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok(json!({ "set": true, "name": name.trim().to_ascii_uppercase() }))
}

async fn delete(cx: Cx, v: Value) -> Out {
    let scope = store::user_scope(&by_a_person(&cx)?);
    let name = crate::ops::text(&v, "name")?;
    if !store::delete(&cx.hub, &scope, &name).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
        return Err((StatusCode::NOT_FOUND, format!("you have no secret {}", name.to_ascii_uppercase())));
    }
    Ok(json!({ "deleted": true }))
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "secrets.mine.list",
            summary: "Your own secrets (the `user` scope): each one's name, size, and when it was set and last used, never its value. A build of one of your Agentfiles is given those it names as `LABEL kiln.secret.<ID>=user:<NAME>`; no one else sees them, the server's owner included.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::SecretsMineList>(),
            route: |_| Ok((Method::GET, path(&["api", "me", "secrets"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "secrets.mine.set",
            summary: "Set one of your own secrets: its `name` (letters, digits and `_`, not starting with a digit) and `value`, kept sealed and never shown again. A person sets their own; an agent can't.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::SecretsMineSet>(),
            route: |v| Ok((Method::PUT, path(&["api", "me", "secrets", &crate::ops::text(v, "name")?]))),
            run: |cx, v| Box::pin(set(cx, v)),
        },
        Op {
            name: "secrets.mine.delete",
            summary: "Delete one of your own secrets by its `name`. A build of an Agentfile naming it is refused from then on.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::SecretsMineDelete>(),
            route: |v| Ok((Method::DELETE, path(&["api", "me", "secrets", &crate::ops::text(v, "name")?]))),
            run: |cx, v| Box::pin(delete(cx, v)),
        },
    ]
}

/// What each of these operations takes (`crate::ops::Input`).
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct SecretsMineList {}

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct SecretsMineSet {
        /// Its name: letters, digits and `_`, not starting with a digit (kept upper-case).
        pub name: String,
        /// Its value, 64 KiB at the most: kept sealed, never shown again.
        pub value: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct SecretsMineDelete {
        /// Its name.
        pub name: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P4.7: an Agentfile names each build secret by its id and one of its person's own by name;
    /// anything else under `kiln.secret.` is refused without its value in the words.
    #[test]
    fn p4_7_an_agentfile_names_its_build_secrets_and_never_their_values() {
        let text = "FROM scratch\nLABEL kiln.secret.GIT_AUTH_TOKEN.github.com=user:gh_token other=1\nLABEL \"kiln.secret.AWS_ACCESS_KEY_ID\"=\"user:AWS_KEY\"\nHARNESS claude FROM git+https://github.com/acme/h.git#0123456789abcdef0123456789abcdef01234567\n";
        let (refs, bad) = references(text);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(refs, [Reference { id: "GIT_AUTH_TOKEN.github.com".into(), name: "GH_TOKEN".into(), given: Given::Secret }, Reference { id: "AWS_ACCESS_KEY_ID".into(), name: "AWS_KEY".into(), given: Given::Secret }]);
        let (refs, bad) = references("FROM scratch\nLABEL kiln.secret.GIT_AUTH_TOKEN=ghp_hunter2value\nLABEL kiln.secret.a,b=user:X\nLABEL kiln.secret.T=user:1BAD\n");
        assert!(refs.is_empty(), "{refs:?}");
        let said: Vec<&str> = bad.iter().map(|m| m.message.as_str()).collect();
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(said.iter().all(|m| !m.contains("ghp_hunter2value")), "{said:?}");
        assert!(said.first().is_some_and(|m| m.contains("user:<NAME>") && m.contains("never goes in an Agentfile")), "{said:?}");
        assert_eq!(bad.first().map(|m| m.location.clone()), Some(vec![(2, 2)]));
        assert!(!format!("{:?}", BuildSecret { id: "GIT_AUTH_TOKEN".into(), value: "ghp_hunter2value".into(), given: Given::Secret }).contains("hunter2"));
    }

    /// P4.6: an SSH key is named as a build secret is, by the agent's id and one of its person's own, beside a build
    /// secret of the same id; its value is refused, not repeated; its build's text names neither; and a build secret from
    /// a server before it reads as one.
    #[test]
    fn p4_6_an_agentfile_names_its_ssh_key_by_reference_alone() -> anyhow::Result<()> {
        let text = "FROM scratch\nLABEL kiln.ssh.default=user:deploy_key kiln.secret.default=user:OTHER keep=1\nHARNESS claude FROM ssh://git@github.com/acme/h.git#0123456789abcdef0123456789abcdef01234567\n";
        let (refs, bad) = references(text);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(refs, [Reference { id: "default".into(), name: "DEPLOY_KEY".into(), given: Given::Ssh }, Reference { id: "default".into(), name: "OTHER".into(), given: Given::Secret }]);
        let (refs, bad) = references("FROM scratch\nLABEL kiln.ssh.default=-----BEGIN_OPENSSH_PRIVATE_KEY-----\nLABEL kiln.ssh.default=user:A kiln.ssh.default=user:B\n");
        assert_eq!(refs, [Reference { id: "default".into(), name: "A".into(), given: Given::Ssh }]);
        let said: Vec<&str> = bad.iter().map(|m| m.message.as_str()).collect();
        assert!(said.len() == 2 && said.iter().all(|m| m.starts_with("LABEL kiln.ssh.default") && !m.contains("BEGIN")), "{said:?}");
        let built = built_text(text);
        assert!(!built.contains("kiln.ssh") && !built.contains("DEPLOY_KEY") && !built.contains("deploy_key") && built.to_ascii_lowercase().contains("label keep=1"), "{built}");
        let old: BuildSecret = serde_json::from_str(r#"{"id":"GIT_AUTH_TOKEN","value":"v"}"#)?;
        assert_eq!(old.given, Given::Secret);
        Ok(())
    }

    /// Shards' store, standing in: a scratch-based plan resolves no base.
    struct Unresolved;

    impl shards_agentfile::plan::Resolver for Unresolved {
        fn resolve(&self, name: &[u8], _: Option<&shards_agentfile::platform::Platform>, _: &[u8]) -> Result<shards_agentfile::plan::Resolved, Vec<u8>> {
            Err([name, b": no base is resolved here"].concat())
        }

        fn epoch(&self, _: &shards_agentfile::plan::EpochSource) -> Result<Option<shards_agentfile::plan::Epoch>, Vec<u8>> {
            Ok(None)
        }
    }

    /// The image config's JSON and the build definition's source maps Shards plans of `text`.
    fn planned(text: &str) -> anyhow::Result<(String, Vec<String>)> {
        use shards_agentfile::{parser, plan, platform::Platform};
        let host = Platform::new("linux", if cfg!(target_arch = "aarch64") { "arm64" } else { "amd64" });
        let opts = plan::Options { target_platform: host.clone(), build_platforms: vec![host], implicit_target: true, dialect: parser::Dialect::Agentfile, image_resolve_mode: b"local".to_vec(), ..plan::Options::default() };
        let p = plan::plan(text.as_bytes(), &opts, &Unresolved).map_err(|e| anyhow::anyhow!("{}", String::from_utf8_lossy(&e.message)))?;
        let config = p.image.to_json().map_err(|e| anyhow::anyhow!("{}", String::from_utf8_lossy(&e)))?;
        let sources = p.definition().sources.iter().map(|s| String::from_utf8_lossy(&s.data).into_owned()).collect();
        Ok((config, sources))
    }

    /// P4.7 (ROOT): a build is given its Agentfile without its references: the image Shards plans of it
    /// has no `kiln.secret.` label and no secret's name in its config, labels or build definition's
    /// source map, while its other labels stay as written and every line keeps its number. Planned
    /// as written, the same text's config carries them: the build's text is what keeps them out.
    #[test]
    fn p4_7_a_built_image_carries_no_reference_and_no_secrets_name() -> anyhow::Result<()> {
        let text = "FROM scratch\nLABEL keep=1 kiln.secret.GIT_AUTH_TOKEN=user:GH_TOKEN \\\n  \"kiln.secret.AWS_ACCESS_KEY_ID\"=\"user:AWS_KEY\" other=\"a b\"\nLABEL kiln.secret.T=user:ONLY_ONE\nLABEL old value with spaces\nENV X=1\n";
        let (as_written, _) = planned(text)?;
        assert!(as_written.contains("kiln.secret.GIT_AUTH_TOKEN") && as_written.contains("GH_TOKEN"), "planned as written, it carries them: {as_written}");
        let built = built_text(text);
        assert_eq!(built.lines().count(), text.lines().count(), "{built}");
        assert_eq!(built.lines().nth(5), Some("ENV X=1"), "{built}");
        let (config, sources) = planned(&built)?;
        for name in ["kiln.secret", "GH_TOKEN", "AWS_KEY", "ONLY_ONE", "GIT_AUTH_TOKEN", "AWS_ACCESS_KEY_ID"] {
            assert!(!config.contains(name) && !built.contains(name) && sources.iter().all(|s| !s.contains(name)), "{name}: {config}\n{built}");
        }
        assert!(config.contains(r#""keep":"1""#) && config.contains(r#""other":"a b""#) && config.contains(r#""old":"value with spaces""#), "its other labels as written: {config}");
        assert_eq!(built_text("FROM scratch\nLABEL a=1\n"), "FROM scratch\nLABEL a=1\n", "a text naming none is as it was");
        Ok(())
    }
}
