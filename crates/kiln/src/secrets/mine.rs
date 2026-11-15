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
//!   (`agentfiles::check`), its words never repeating what was written.

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ci::secrets as store;
use crate::ops::{Cx, Fail, Input, Op, Out, path};

/// What an Agentfile's label naming a build secret starts with: `LABEL kiln.secret.<ID>=user:<NAME>`.
pub const LABEL: &str = "kiln.secret.";

/// What a reference's value starts with: the scope it's read from, a person's own.
pub const USER: &str = "user:";

/// One secret a build of a person's own Agentfile is given: Shards' id for it, and its value, read
/// from that person's own by kiln's server as the session started. Its value is never shown:
/// `Debug` names its id alone.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSecret {
    pub id: String,
    pub value: String,
}

impl std::fmt::Debug for BuildSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildSecret").field("id", &self.id).finish_non_exhaustive()
    }
}

/// One `LABEL kiln.secret.<ID>=user:<NAME>` of an Agentfile: the build secret, and whose own it's read
/// from by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub id: String,
    pub name: String,
}

/// Why a `kiln.secret.` label isn't a reference, and where: its words never repeat its value.
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

/// The build secrets `text` names (`LABEL kiln.secret.<ID>=user:<NAME>`, in any stage), each once by
/// its id, and each `kiln.secret.` label that isn't one, with why. Read with Shards' own parser; a
/// text it can't read names none (its check says why).
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
            let Some(id) = unquoted(&key).strip_prefix(LABEL).map(str::to_owned) else { continue };
            let value = String::from_utf8_lossy(&kv.value);
            let name = unquoted(&value).strip_prefix(USER).map(str::to_owned);
            let said = match name.as_deref().map(kiln_ci::secrets::check_name) {
                _ if !build_id(&id) => Some(format!("LABEL {LABEL}{id}: a build secret's id is letters, digits, `_`, `.` and `-` (`GIT_AUTH_TOKEN`, `GIT_AUTH_TOKEN.github.com`)")),
                Some(Ok(_)) if refs.iter().any(|r| r.id == id) => Some(format!("LABEL {LABEL}{id} names its secret twice: once is all a build takes")),
                Some(Ok(name)) => {
                    refs.push(Reference { id, name });
                    None
                }
                Some(Err(why)) => Some(format!("LABEL {LABEL}{id}: user:<NAME> names one of your secrets, and {why}")),
                None => Some(format!("LABEL {LABEL}{id} names its secret as user:<NAME>, one of your own (set at /api/me/secrets): a secret's value never goes in an Agentfile")),
            };
            if let Some(message) = said {
                bad.push(Misnamed { message, location: location.clone() });
            }
        }
    }
    (refs, bad)
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
        let named: Vec<String> = refs.iter().filter(|r| missing.contains(&r.name)).map(|r| format!("{LABEL}{} names {USER}{}", r.id, r.name)).collect();
        return Err(format!("its Agentfile's {}, which isn't one of your secrets: set it at /api/me/secrets (secrets.mine.set)", named.join(", ")));
    }
    Ok(refs.into_iter().filter_map(|r| held.get(&r.name).map(|v| BuildSecret { id: r.id, value: v.clone() })).collect())
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
        assert_eq!(refs, [Reference { id: "GIT_AUTH_TOKEN.github.com".into(), name: "GH_TOKEN".into() }, Reference { id: "AWS_ACCESS_KEY_ID".into(), name: "AWS_KEY".into() }]);
        let (refs, bad) = references("FROM scratch\nLABEL kiln.secret.GIT_AUTH_TOKEN=ghp_hunter2value\nLABEL kiln.secret.a,b=user:X\nLABEL kiln.secret.T=user:1BAD\n");
        assert!(refs.is_empty(), "{refs:?}");
        let said: Vec<&str> = bad.iter().map(|m| m.message.as_str()).collect();
        assert_eq!(said.len(), 3, "{said:?}");
        assert!(said.iter().all(|m| !m.contains("ghp_hunter2value")), "{said:?}");
        assert!(said.first().is_some_and(|m| m.contains("user:<NAME>") && m.contains("never goes in an Agentfile")), "{said:?}");
        assert_eq!(bad.first().map(|m| m.location.clone()), Some(vec![(2, 2)]));
        assert!(!format!("{:?}", BuildSecret { id: "GIT_AUTH_TOKEN".into(), value: "ghp_hunter2value".into() }).contains("hunter2"));
    }
}
