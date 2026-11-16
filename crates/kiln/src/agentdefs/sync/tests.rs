//! An agent definition and its Agentfile's `AGENT` kept consistent both ways (kiln plan
//! 01a1dd2e8618733a87f2793aab1322a8 P5.4's tests "edit the agent", "edit the directive" and "concurrent
//! edits"), through the operation table as REST, MCP and code mode call it (`crate::ops::call`).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::agentfiles::settings::{self, AgentSettings, Remote};
use crate::authz::{Caller, Grant, Rights, gate::Kind};
use crate::hub::Hub;
use crate::ops::{Cx, Fail};

const TEXT: &str = "FROM ubuntu:24.04\nHARNESS claude FROM ./harness\nAGENT reviewer FROM ./agent\nATTACH reviewer FOR claude\nCMD [\"claude\"]\n";

const GITHUB: &str = "https://api.githubcopilot.com/mcp/";

/// A person, `user:<name>`, an admin here.
fn person(name: &str) -> anyhow::Result<Caller> {
    let admin = Grant::parse("admin").map_err(anyhow::Error::msg)?;
    let mut caller = Caller::with(
        Kind::User,
        &format!("user:{name}"),
        Some(Rights {
            grants: vec![admin],
            ..Default::default()
        }),
        None,
    );
    caller.user = Some(name.to_owned());
    Ok(caller)
}

async fn op(hub: &Arc<Hub>, caller: &Caller, name: &str, input: Value) -> Result<Value, Fail> {
    crate::ops::call(
        Cx {
            hub: Arc::clone(hub),
            caller: caller.clone(),
        },
        name,
        input,
    )
    .await
}

fn failed((code, why): Fail) -> anyhow::Error {
    anyhow::anyhow!("{code}: {why}")
}

fn text_at(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn number_at(v: &Value, pointer: &str) -> i64 {
    v.pointer(pointer)
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

/// What Agentfile text `t` says of agent `reviewer`.
fn reviewer(t: &str) -> anyhow::Result<AgentSettings> {
    let s = settings::of(t)?;
    s.agents
        .iter()
        .find(|a| a.name == "reviewer")
        .map(AgentSettings::from)
        .ok_or_else(|| anyhow::anyhow!("no AGENT reviewer in {t}"))
}

/// Ada's Agentfile declaring `AGENT reviewer`, and her agent definition `reviewer` naming it, each at
/// version 1: their ids, and whose they are.
async fn linked(hub: &Arc<Hub>, ada: &Caller) -> anyhow::Result<(String, String, String)> {
    let f = op(
        hub,
        ada,
        "agentfiles.create",
        json!({"name": "review-box", "text": TEXT}),
    )
    .await
    .map_err(failed)?;
    let fid = text_at(&f, "/agentfile/id");
    let d = op(
        hub,
        ada,
        "agentdefs.create",
        json!({"name": "reviewer", "harness": "claude", "agentfile": format!("agentfile:{fid}")}),
    )
    .await
    .map_err(failed)?;
    anyhow::ensure!(number_at(&d, "/version") == 1, "{d}");
    let f = op(hub, ada, "agentfiles.get", json!({"id": fid}))
        .await
        .map_err(failed)?;
    anyhow::ensure!(
        number_at(&f, "/version") == 1 && text_at(&f, "/text") == TEXT,
        "nothing to share yet, nothing written: {f}"
    );
    Ok((fid, text_at(&d, "/id"), text_at(&f, "/owner")))
}

/// P5.4's test "edit the agent": Ada's agent takes a model, tools, skills and two MCP servers; its
/// Agentfile is a new version whose `AGENT reviewer` says the same (kiln's labels, Shards' `MCP … FOR`),
/// which Shards passes, but for the server a command starts, which is the definition's alone. A change of
/// what they don't share (its instructions) writes no new version of the Agentfile. Taking the sync out of
/// `agentdefs.update` leaves the Agentfile at version 1.
#[test]
fn p5_4_edit_the_agent_and_its_directive_is_rewritten() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("ada")?;
        let (fid, did, _) = linked(&hub, &ada).await?;
        let mcp = json!([{"name": "github", "url": GITHUB}, {"name": "local", "command": ["npx", "server"]}]);
        let input = json!({"id": did, "model": "opus", "tools": ["Read", "Bash(git diff:*)"], "skills": ["kiln-ci-author"], "mcp": mcp, "base": 1});
        let d = op(&hub, &ada, "agentdefs.update", input)
            .await
            .map_err(failed)?;
        anyhow::ensure!(number_at(&d, "/version") == 2, "{d}");
        let f = op(&hub, &ada, "agentfiles.get", json!({"id": fid}))
            .await
            .map_err(failed)?;
        let text = text_at(&f, "/text");
        anyhow::ensure!(
            number_at(&f, "/version") == 2,
            "its Agentfile is a new version: {f}"
        );
        let want = AgentSettings {
            model: Some("opus".into()),
            tools: vec!["Read".into(), "Bash(git diff:*)".into()],
            skills: vec!["kiln-ci-author".into()],
            mcp: vec![Remote {
                name: "github".into(),
                url: GITHUB.into(),
            }],
        };
        assert_eq!(reviewer(&text)?, want);
        anyhow::ensure!(
            !text.contains("npx"),
            "a server a command starts is the definition's alone:\n{text}"
        );
        let c = crate::agentfiles::check::lint(&text);
        anyhow::ensure!(c.checked && c.refusal().is_none(), "{c:?}\n{text}");

        op(
            &hub,
            &ada,
            "agentdefs.update",
            json!({"id": did, "instructions": "Review it.", "base": 2}),
        )
        .await
        .map_err(failed)?;
        let f = op(&hub, &ada, "agentfiles.get", json!({"id": fid}))
            .await
            .map_err(failed)?;
        anyhow::ensure!(
            number_at(&f, "/version") == 2,
            "what they don't share writes nothing: {f}"
        );
        Ok(())
    })
}

/// P5.4's test "edit the directive": Ada's Agentfile text gives `AGENT reviewer` tools, a model and an MCP
/// server; her agent takes them as a new version, which the change's answer names (`synced`), and keeps
/// its own server a command starts. One her agent can't take (a tool with no name) is refused typed, and
/// changes neither. Taking the sync out of `agentfiles.update` leaves the agent at version 1.
#[test]
fn p5_4_edit_the_directive_and_the_agent_is_updated() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("ada")?;
        let (fid, did, _) = linked(&hub, &ada).await?;
        op(
            &hub,
            &ada,
            "agentdefs.update",
            json!({"id": did, "mcp": [{"name": "local", "command": ["npx", "server"]}], "base": 1}),
        )
        .await
        .map_err(failed)?;
        let said = format!(
            "AGENT reviewer FROM ./agent\nLABEL kiln.agent.reviewer.tools='[\"Read\",\"Grep\"]' kiln.agent.reviewer.model=haiku\nMCP github FROM {GITHUB} FOR reviewer"
        );
        let text = TEXT.replace("AGENT reviewer FROM ./agent", &said);
        let saved = op(
            &hub,
            &ada,
            "agentfiles.update",
            json!({"id": fid, "text": text, "base": 1}),
        )
        .await
        .map_err(failed)?;
        anyhow::ensure!(
            number_at(&saved, "/agentfile/version") == 2
                && text_at(&saved, "/agentfile/text") == text,
            "kept as written: {saved}"
        );
        anyhow::ensure!(
            saved.pointer("/synced")
                == Some(&json!([{"id": did, "name": "reviewer", "version": 3}])),
            "{saved}"
        );
        let d = op(&hub, &ada, "agentdefs.get", json!({"id": did}))
            .await
            .map_err(failed)?;
        anyhow::ensure!(
            d.pointer("/tools") == Some(&json!(["Read", "Grep"]))
                && text_at(&d, "/model") == "haiku",
            "{d}"
        );
        anyhow::ensure!(
            d.pointer("/mcp")
                == Some(
                    &json!([{"name": "github", "url": GITHUB}, {"name": "local", "command": ["npx", "server"]}])
                ),
            "{d}"
        );

        let nameless = text.replace("[\"Read\",\"Grep\"]", "[\"Read\",\"\"]");
        let refused = op(
            &hub,
            &ada,
            "agentfiles.update",
            json!({"id": fid, "text": nameless, "base": 2}),
        )
        .await;
        anyhow::ensure!(
            matches!(&refused, Err((http::StatusCode::UNPROCESSABLE_ENTITY, why)) if why.contains("agent reviewer can't take") && why.contains("a tool needs its name")),
            "{refused:?}"
        );
        let f = op(&hub, &ada, "agentfiles.get", json!({"id": fid}))
            .await
            .map_err(failed)?;
        let d = op(&hub, &ada, "agentdefs.get", json!({"id": did}))
            .await
            .map_err(failed)?;
        anyhow::ensure!(
            number_at(&f, "/version") == 2 && number_at(&d, "/version") == 3,
            "neither changed: {f} {d}"
        );
        Ok(())
    })
}

/// P5.4's test "concurrent edits": two changes of the Agentfile over its version 1, the second refused
/// (409) saying the version it is now; and a setting both sides changed differently since they last agreed
/// (the Agentfile's text changed where its agent didn't hear of it, then the agent otherwise, and then the
/// other way round) refused whole (409), naming it and each side's value, neither side changed. Taking the
/// base out of `agentfiles::update` keeps the second change over the first; making `pick` take ours when
/// both changed overwrites the Agentfile's.
#[test]
fn p5_4_concurrent_edits_are_shown_as_a_conflict() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let ada = person("ada")?;
        let (fid, did, owner) = linked(&hub, &ada).await?;
        op(
            &hub,
            &ada,
            "agentfiles.update",
            json!({"id": fid, "text": format!("{TEXT}LABEL a=1\n"), "base": 1}),
        )
        .await
        .map_err(failed)?;
        let second = op(
            &hub,
            &ada,
            "agentfiles.update",
            json!({"id": fid, "text": format!("{TEXT}LABEL b=2\n"), "base": 1}),
        )
        .await;
        anyhow::ensure!(
            matches!(&second, Err((http::StatusCode::CONFLICT, why)) if why.contains("since version 1") && why.contains("version 2 now")),
            "{second:?}"
        );

        // The Agentfile's AGENT given tools behind its agent's back (kiln's own write, no sync), then the agent other tools.
        let who = crate::agentfiles::Who {
            person: owner,
            all: false,
        };
        let theirs = format!("{TEXT}LABEL a=1 kiln.agent.reviewer.tools='[\"Grep\"]'\n");
        crate::agentfiles::update(
            &hub,
            hub.blobs.blobs.as_ref(),
            &who,
            &fid,
            crate::agentfiles::Patch {
                name: None,
                text: Some(theirs.clone()),
            },
            None,
            "test",
        )
        .await?;
        let refused = op(
            &hub,
            &ada,
            "agentdefs.update",
            json!({"id": did, "tools": ["Read"], "base": 1}),
        )
        .await;
        anyhow::ensure!(
            matches!(&refused, Err((http::StatusCode::CONFLICT, why)) if why.contains("reviewer's tools: the agent definition says [\"Read\"], its Agentfile's AGENT [\"Grep\"]")),
            "{refused:?}"
        );
        let f = op(&hub, &ada, "agentfiles.get", json!({"id": fid}))
            .await
            .map_err(failed)?;
        let d = op(&hub, &ada, "agentdefs.get", json!({"id": did}))
            .await
            .map_err(failed)?;
        anyhow::ensure!(
            number_at(&f, "/version") == 3
                && text_at(&f, "/text") == theirs
                && number_at(&d, "/version") == 1,
            "neither changed: {f} {d}"
        );

        // And the other way: the agent's tools changed behind its Agentfile's back, then the Agentfile's otherwise.
        let dwho = super::Who {
            person: text_at(&d, "/owner"),
            all: false,
        };
        super::super::update(
            &hub,
            &dwho,
            &did,
            super::Patch {
                tools: Some(vec!["Glob".into()]),
                ..super::Patch::default()
            },
            None,
            "test",
        )
        .await?;
        let other = theirs.replace("[\"Grep\"]", "[\"WebFetch\"]");
        let refused = op(
            &hub,
            &ada,
            "agentfiles.update",
            json!({"id": fid, "text": other, "base": 3}),
        )
        .await;
        anyhow::ensure!(
            matches!(&refused, Err((http::StatusCode::CONFLICT, why)) if why.contains("the agent definition says [\"Glob\"], its Agentfile's AGENT [\"WebFetch\"]")),
            "{refused:?}"
        );
        let f = op(&hub, &ada, "agentfiles.get", json!({"id": fid}))
            .await
            .map_err(failed)?;
        anyhow::ensure!(
            number_at(&f, "/version") == 3,
            "the Agentfile didn't change: {f}"
        );
        Ok(())
    })
}

/// The three-way merge: a setting only one side changed takes that side's; one both changed alike is kept;
/// one both changed differently is named, with each side's value, as the definition and its Agentfile say it.
#[test]
fn p5_4_each_setting_is_merged_from_what_the_changed_side_said_before() {
    let s = |tools: &[&str], model: Option<&str>| AgentSettings {
        tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        model: model.map(str::to_owned),
        ..AgentSettings::default()
    };
    let base = s(&["Read"], None);
    assert_eq!(
        super::merge(
            "a",
            &base,
            &s(&["Read", "Grep"], None),
            &s(&["Read"], Some("m")),
            super::Changed::Definition
        ),
        Ok(s(&["Read", "Grep"], Some("m")))
    );
    assert_eq!(
        super::merge(
            "a",
            &base,
            &s(&["Grep"], None),
            &s(&["Grep"], None),
            super::Changed::Agentfile
        ),
        Ok(s(&["Grep"], None))
    );
    let both = super::merge(
        "a",
        &base,
        &s(&["Grep"], None),
        &s(&["Bash"], None),
        super::Changed::Agentfile,
    );
    assert_eq!(
        both,
        Err(vec![super::Diverged {
            agent: "a".into(),
            setting: "tools",
            definition: json!(["Bash"]),
            agentfile: json!(["Grep"])
        }])
    );
}
