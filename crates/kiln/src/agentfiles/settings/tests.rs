//! An Agentfile's settings read from its text and written back into it (kiln plan
//! 01a1dd2e8618733a87f2793aab1322a8 P5.3, P5.4), each change held to Shards' own check of the text it makes
//! (`check::lint`).

use super::*;
use crate::agentfiles::builtin;
use crate::agentfiles::check;

fn claude() -> anyhow::Result<&'static str> {
    builtin::ALL
        .iter()
        .find(|b| b.harness == "claude")
        .map(|b| b.text)
        .ok_or_else(|| anyhow::anyhow!("no built-in for claude"))
}

/// What Shards says of `t`: refused, in its words, where it refuses it.
fn passes(t: &str) -> anyhow::Result<()> {
    let c = check::lint(t);
    anyhow::ensure!(
        c.checked && c.refusal().is_none(),
        "Shards refuses it: {c:?}\n{t}"
    );
    Ok(())
}

fn harness<'a>(s: &'a Settings, name: &str) -> anyhow::Result<&'a Harness> {
    s.harnesses
        .iter()
        .find(|h| h.name == name)
        .ok_or_else(|| anyhow::anyhow!("no harness {name}: {s:?}"))
}

/// The lines `after` has that `before` doesn't, and those it lost, in order.
fn diff(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
    let (b, a): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    (
        a.iter()
            .filter(|l| !b.contains(l))
            .map(|l| (*l).to_owned())
            .collect(),
        b.iter()
            .filter(|l| !a.contains(l))
            .map(|l| (*l).to_owned())
            .collect(),
    )
}

/// Every Agentfile kiln ships reads as its harness's settings: its source as written, no version (a path
/// names none), no model or sign-in of kiln's, its HOME volume, and the hosts it may reach.
#[test]
fn p5_3_every_builtins_harness_reads_as_its_settings() -> anyhow::Result<()> {
    for b in builtin::ALL {
        let s = of(b.text)?;
        let h = harness(&s, b.harness)?;
        anyhow::ensure!(
            h.source.starts_with("./")
                && h.version.is_none()
                && h.model.is_none()
                && h.sign_in.is_none(),
            "{}: {h:?}",
            b.name
        );
        anyhow::ensure!(
            h.volumes == [format!("/home/{}", b.harness)],
            "{}: {h:?}",
            b.name
        );
        anyhow::ensure!(
            !h.egress.is_empty() && h.egress.iter().all(|e| e.ends_with(":443")),
            "{}: {h:?}",
            b.name
        );
    }
    let h = harness(&of(claude()?)?, "claude")?.clone();
    assert_eq!(h.source, "./claude-${TARGETARCH}-${CLAUDE_LIBC}");
    assert_eq!(h.volumes, ["/home/claude"]);
    assert_eq!(
        h.egress,
        [
            "api.anthropic.com:443",
            "claude.ai:443",
            "platform.claude.com:443"
        ]
    );
    Ok(())
}

/// P5.3's test "set a model in the view": the text gains `LABEL kiln.harness.claude.model=…` after its
/// `HARNESS`, and nothing else changes; Shards' check passes; the view reads it back. Changed again, the
/// label is rewritten where it is; taken out, the text is as it was.
#[test]
fn p5_3_set_a_model_in_the_view_and_the_text_changes() -> anyhow::Result<()> {
    let text = claude()?;
    let set = set_harness(
        text,
        "claude",
        &HarnessChange {
            model: Some(Some("claude-opus-4-1".into())),
            ..HarnessChange::default()
        },
    )?;
    let (added, lost) = diff(text, &set);
    anyhow::ensure!(
        added == ["LABEL kiln.harness.claude.model=claude-opus-4-1"] && lost.is_empty(),
        "{added:?} {lost:?}"
    );
    let at = set
        .lines()
        .position(|l| l.starts_with("HARNESS claude"))
        .ok_or_else(|| anyhow::anyhow!("no HARNESS"))?;
    anyhow::ensure!(
        set.lines().nth(at + 1) == Some("LABEL kiln.harness.claude.model=claude-opus-4-1"),
        "{set}"
    );
    passes(&set)?;
    assert_eq!(
        harness(&of(&set)?, "claude")?.model.as_deref(),
        Some("claude-opus-4-1")
    );

    let again = set_harness(
        &set,
        "claude",
        &HarnessChange {
            model: Some(Some("claude-sonnet-4-5[1m]".into())),
            ..HarnessChange::default()
        },
    )?;
    let (added, lost) = diff(&set, &again);
    anyhow::ensure!(
        added == ["LABEL kiln.harness.claude.model=claude-sonnet-4-5[1m]"]
            && lost == ["LABEL kiln.harness.claude.model=claude-opus-4-1"],
        "{added:?} {lost:?}"
    );
    passes(&again)?;
    let out = set_harness(
        &again,
        "claude",
        &HarnessChange {
            model: Some(None),
            ..HarnessChange::default()
        },
    )?;
    assert_eq!(out, text, "taken out, the text is as it was");
    Ok(())
}

/// P5.3's test "edit the text": the view is read from the text, so a label, a volume or a destination
/// written in it is the view's at once, in any way a build reads it (quoted, among other labels).
#[test]
fn p5_3_edit_the_text_and_the_view_updates() -> anyhow::Result<()> {
    let text = claude()?
        .replace("HARNESS claude FROM ./claude-${TARGETARCH}-${CLAUDE_LIBC}", "HARNESS claude FROM ./claude-${TARGETARCH}-${CLAUDE_LIBC}\nLABEL team=core \"kiln.harness.claude.model\"='claude-haiku-4-5' kiln.harness.claude.sign-in=claude/work")
        .replace("VOLUME --target-kind=harness /home/claude FOR claude", "VOLUME --target-kind=harness /home/claude /cache FOR claude")
        .replace("--egress=claude.ai:443 ", "");
    passes(&text)?;
    let h = harness(&of(&text)?, "claude")?.clone();
    assert_eq!(
        (h.model.as_deref(), h.sign_in.as_deref()),
        (Some("claude-haiku-4-5"), Some("claude/work"))
    );
    assert_eq!(h.volumes, ["/home/claude", "/cache"]);
    assert_eq!(
        h.egress,
        ["api.anthropic.com:443", "platform.claude.com:443"]
    );
    // Set from the view over what the text wrote: its other label stays, its own pair is rewritten in place.
    let set = set_harness(
        &text,
        "claude",
        &HarnessChange {
            model: Some(Some("claude-opus-4-1".into())),
            ..HarnessChange::default()
        },
    )?;
    let (added, _) = diff(&text, &set);
    assert_eq!(
        added,
        [
            "LABEL team=core kiln.harness.claude.model=claude-opus-4-1 kiln.harness.claude.sign-in=claude/work"
        ]
    );
    passes(&set)
}

/// Each of a harness's settings, set in the view on every Agentfile kiln ships, reads back as it was set
/// and Shards passes the text it makes: sign-in, volumes, egress and model at once.
#[test]
fn p5_3_every_setting_round_trips_through_the_text_on_every_builtin() -> anyhow::Result<()> {
    for b in builtin::ALL {
        let h = b.harness;
        let change = HarnessChange {
            model: Some(Some("some-model".into())),
            sign_in: Some(Some(format!("{h}/work"))),
            volumes: Some(vec![format!("/home/{h}"), "/var/cache/tools".into()]),
            egress: Some(vec![
                "api.github.com:443".into(),
                "proxy.golang.org:443".into(),
                "registry.example.com:5000".into(),
            ]),
            ..HarnessChange::default()
        };
        let set = set_harness(b.text, h, &change)?;
        passes(&set).map_err(|e| anyhow::anyhow!("{}: {e}", b.name))?;
        let got = harness(&of(&set)?, h)?.clone();
        anyhow::ensure!(
            got.model.as_deref() == Some("some-model") && got.sign_in == Some(format!("{h}/work")),
            "{}: {got:?}",
            b.name
        );
        anyhow::ensure!(
            got.volumes == [format!("/home/{h}"), "/var/cache/tools".into()],
            "{}: {got:?}",
            b.name
        );
        anyhow::ensure!(
            got.egress
                == [
                    "api.github.com:443",
                    "proxy.golang.org:443",
                    "registry.example.com:5000"
                ],
            "{}: {got:?}",
            b.name
        );
        // The microVM's boundary opens each port its destinations take, once.
        anyhow::ensure!(
            set.lines().any(|l| l.starts_with(&format!(
                "EXPOSE --harnesses={h} 443/tcp 5000/tcp AS egress FOR "
            ))),
            "{}:\n{set}",
            b.name
        );
    }
    Ok(())
}

/// A harness with no egress gets a network of its own, its boundary and its joining it, as kiln's
/// Agentfiles write them, and Shards passes it; its egress taken out again, those lines go and the text
/// is as it was. One whose egress is on a network it shares is changed in the text, typed.
#[test]
fn p5_3_egress_makes_and_takes_out_a_harness_network() -> anyhow::Result<()> {
    let text = "FROM ubuntu:24.04\nHARNESS codex FROM ./codex\nCMD [\"codex\"]\n";
    let set = set_harness(
        text,
        "codex",
        &HarnessChange {
            egress: Some(vec!["api.openai.com:443".into()]),
            ..HarnessChange::default()
        },
    )?;
    assert_eq!(
        set,
        "FROM ubuntu:24.04\nHARNESS codex FROM ./codex\nNETWORK --egress=api.openai.com:443 codex-egress FOR codex\nEXPOSE --harnesses=codex 443/tcp AS egress FOR codex-egress\nCONNECT codex WITH codex ON codex-egress\nCMD [\"codex\"]\n"
    );
    passes(&set)?;
    assert_eq!(harness(&of(&set)?, "codex")?.egress, ["api.openai.com:443"]);
    assert_eq!(
        set_harness(
            &set,
            "codex",
            &HarnessChange {
                egress: Some(Vec::new()),
                ..HarnessChange::default()
            }
        )?,
        text
    );

    let shared = "FROM ubuntu:24.04\nHARNESS codex FROM ./codex\nHARNESS claude FROM ./claude\nNETWORK --egress=api.openai.com:443 out FOR codex claude\nEXPOSE 443/tcp AS egress FOR out\nCONNECT --port=1/tcp codex WITH claude ON out\n";
    let refused = set_harness(
        shared,
        "codex",
        &HarnessChange {
            egress: Some(vec!["chatgpt.com:443".into()]),
            ..HarnessChange::default()
        },
    );
    anyhow::ensure!(
        matches!(&refused, Err(Refusal::InText(why)) if why.contains("network out, which it shares")),
        "{refused:?}"
    );
    Ok(())
}

/// A version is what its source names apart from where it is: an OCI tag or digest, a Git ref, an S3
/// object's version id, each set in place; a path's or an http(s) URL's is its source's, refused typed.
#[test]
fn p5_3_version_and_source_are_set_on_the_harness_line() -> anyhow::Result<()> {
    let line = |source: &str| {
        format!("FROM ubuntu:24.04\nHARNESS --processes=8 claude FROM {source} TO /opt/claude\n")
    };
    let version = |t: &str| -> anyhow::Result<Option<String>> {
        Ok(harness(&of(t)?, "claude")?.version.clone())
    };
    let to = |v: Option<&str>| HarnessChange {
        version: Some(v.map(str::to_owned)),
        ..HarnessChange::default()
    };

    let oci = line("ghcr.io/acme/claude:2.1.300");
    assert_eq!(version(&oci)?.as_deref(), Some("2.1.300"));
    let set = set_harness(&oci, "claude", &to(Some("2.1.311")))?;
    assert_eq!(
        set,
        line("ghcr.io/acme/claude:2.1.311"),
        "its flags and TO stay as written"
    );
    let pinned = set_harness(
        &oci,
        "claude",
        &to(Some(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )),
    )?;
    assert_eq!(
        version(&pinned)?.as_deref(),
        Some("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        set_harness(&line("localhost:5000/claude:1"), "claude", &to(Some("2")))?,
        line("localhost:5000/claude:2"),
        "a registry's port isn't a tag"
    );

    let git = line("https://github.com/acme/claude.git#v1:dist");
    assert_eq!(version(&git)?.as_deref(), Some("v1"));
    assert_eq!(
        set_harness(&git, "claude", &to(Some("v2")))?,
        line("https://github.com/acme/claude.git#v2:dist")
    );

    let s3 = "FROM ubuntu:24.04\nHARNESS --version-id=a1 claude FROM s3://bucket/claude.tgz\n";
    assert_eq!(version(s3)?.as_deref(), Some("a1"));
    let set = set_harness(s3, "claude", &to(Some("b2")))?;
    assert_eq!(
        set,
        "FROM ubuntu:24.04\nHARNESS --version-id=b2 claude FROM s3://bucket/claude.tgz\n"
    );
    assert_eq!(version(&set)?.as_deref(), Some("b2"));

    let http = format!(
        "FROM ubuntu:24.04\nHARNESS --checksum=sha256:{} claude FROM https://example.com/claude.tgz\n",
        "0".repeat(64)
    );
    for t in [line("./claude"), http] {
        let refused = set_harness(&t, "claude", &to(Some("2")));
        anyhow::ensure!(
            matches!(
                &refused,
                Err(Refusal::Value {
                    setting: "version",
                    ..
                })
            ),
            "{t}: {refused:?}"
        );
    }
    let moved = set_harness(
        &line("./claude"),
        "claude",
        &HarnessChange {
            source: Some("ghcr.io/acme/claude:3".into()),
            ..HarnessChange::default()
        },
    )?;
    assert_eq!(moved, line("ghcr.io/acme/claude:3"));
    passes(&moved)
}

/// What a change can't be is refused typed, the text untouched: no such harness, a model that isn't one
/// word a build reads as written, a sign-in that isn't this harness's by its name (`claude/<name>`), a
/// destination that isn't HOST:PORT.
#[test]
fn p5_3_a_change_kiln_cant_make_is_refused_typed() -> anyhow::Result<()> {
    let text = claude()?;
    let refused = |name: &str, c: HarnessChange| set_harness(text, name, &c);
    anyhow::ensure!(matches!(
        refused(
            "codex",
            HarnessChange {
                model: Some(Some("m".into())),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Undeclared {
            what: "harness",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        refused(
            "claude",
            HarnessChange {
                model: Some(Some("two words".into())),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Value {
            setting: "model",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        refused(
            "claude",
            HarnessChange {
                model: Some(Some("$MODEL".into())),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Value {
            setting: "model",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        refused(
            "claude",
            HarnessChange {
                sign_in: Some(Some("sk-ant/xyz=".into())),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Value {
            setting: "sign-in",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        refused(
            "claude",
            HarnessChange {
                sign_in: Some(Some("codex/ada".into())),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Value {
            setting: "sign-in",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        refused(
            "claude",
            HarnessChange {
                egress: Some(vec!["443".into()]),
                ..HarnessChange::default()
            }
        ),
        Err(Refusal::Value {
            setting: "egress",
            ..
        })
    ));
    anyhow::ensure!(matches!(
        of("FROM ubuntu:24.04\nFORM x\n"),
        Err(Refusal::Unread(_))
    ));
    Ok(())
}

/// P5.4's shared settings: an agent's model, tools and skills as kiln's labels, its remote MCP servers as
/// Shards' `MCP … FOR <agent>`, written after its `AGENT`, read back as written, and Shards passes them;
/// written again they're rewritten in place, and taken out the text is as it was. An MCP server from
/// another source is its image's own, and stays.
#[test]
fn p5_4_an_agents_settings_round_trip_through_its_directives() -> anyhow::Result<()> {
    let text = "FROM ubuntu:24.04\nHARNESS claude FROM ./claude\nAGENT reviewer FROM ./reviewer\nMCP fs FROM ./fs-server FOR reviewer\nATTACH reviewer FOR claude\n";
    passes(text)?;
    let s = AgentSettings {
        model: Some("claude-opus-4-1".into()),
        tools: vec!["Read".into(), "Bash(git diff:*)".into(), "it's".into()],
        skills: vec!["kiln-ci-author".into()],
        mcp: vec![Remote {
            name: "github".into(),
            url: "https://api.githubcopilot.com/mcp/".into(),
        }],
    };
    let set = set_agent(text, "reviewer", &s)?;
    passes(&set)?;
    let read = of(&set)?;
    let a = read
        .agents
        .iter()
        .find(|a| a.name == "reviewer")
        .ok_or_else(|| anyhow::anyhow!("no agent: {read:?}"))?;
    assert_eq!(AgentSettings::from(a), s);
    anyhow::ensure!(
        set.contains("MCP fs FROM ./fs-server FOR reviewer")
            && set.contains("MCP github FROM https://api.githubcopilot.com/mcp/ FOR reviewer"),
        "{set}"
    );

    let fewer = AgentSettings {
        tools: vec!["Read".into()],
        mcp: Vec::new(),
        ..s.clone()
    };
    let again = set_agent(&set, "reviewer", &fewer)?;
    passes(&again)?;
    let (added, lost) = diff(&set, &again);
    anyhow::ensure!(
        added.len() == 1
            && added
                .iter()
                .all(|l| l.contains("kiln.agent.reviewer.tools='[\"Read\"]'"))
            && lost.len() == 2,
        "{added:?} {lost:?}"
    );
    assert_eq!(
        set_agent(&again, "reviewer", &AgentSettings::default())?,
        text
    );
    anyhow::ensure!(matches!(
        set_agent(text, "nobody", &s),
        Err(Refusal::Undeclared { what: "agent", .. })
    ));
    Ok(())
}
