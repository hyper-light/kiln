//! Every kiln harness in a Shards microVM of its own, through kiln's API (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 35; plan 01a1d2c16eb074f19cf7ae7f1025c0be step 2; its UI half
//! is `ui/e2e/vm-harnesses.spec.ts`). A part of `vm_harnesses.rs`, on step 34's drive (`vm_e2e`):
//!
//! - `checks`: `gaps.json`, each harness's gaps with their cited reasons, held to kiln's own code and
//!   to the sources it cites, and the rules a run holds itself to (any OS).
//! - `host`: the eight harnesses step 34 doesn't drive, each through kiln as a process on this host,
//!   the stand-in behind its name (Unix: the stand-in is a Python script).
//! - `rest`: the ten, each in a microVM from kiln's Agentfile and from a person's own, through kiln's
//!   REST API: start, steer, interrupt, stop, resume, kiln's MCP from inside, an agent started there;
//!   and two Antigravity agents of one person on one machine, each reaching kiln's MCP as itself.
//! - `mcp` (`mcp.rs`): the ten through kiln's MCP server, as a person's or an agent's client drives
//!   them: `agents_start` naming the Agentfile, `agents_send` steering, `agents_interrupt`,
//!   `agents_stop`, and `agents_start` taking the conversation up again (`resume`); each as its own
//!   tool, and in code mode (`code_run`'s `kiln.agents.start`).
//!
//! `rest::` and `mcp::` skip, saying why, until the runner a test starts says it starts microVMs
//! (`facts.virt.microvm`); `VM_E2E=1` makes that a failure. A vendor's own CLI, from kiln's Agentfile
//! or a person's own made from it, takes ROOT's task alone (`vm_e2e::TASK_WORDS`): never the
//! stand-in's words. ROOT runs them: `tests/vm_e2e/README.md`.

use serde_json::Value;

use crate::vm_e2e;

#[path = "mcp.rs"]
mod mcp;

/// `gaps.json`: what each harness does through kiln on the host and in a microVM, each gap with its
/// sources.
fn gaps() -> anyhow::Result<Value> {
    Ok(serde_json::from_str(include_str!("gaps.json"))?)
}

/// Whether `kind`'s row says `flag` holds; one not recorded yet (null) is tried, so ROOT's run says.
fn tried(gaps: &Value, kind: &str, flag: &str) -> bool {
    gaps.pointer(&format!("/{kind}/{flag}")).is_some_and(|v| v.as_bool() != Some(false))
}

/// Each gap of `kind`'s that a run of `flag` passes by, said where the run's output shows it.
fn say_gaps(gaps: &Value, kind: &str, flag: &str) {
    for g in gaps.pointer(&format!("/{kind}/gaps")).and_then(Value::as_array).into_iter().flatten() {
        if g.get("what").and_then(Value::as_str) == Some(flag) {
            eprintln!("GAP: {kind}: {flag}: {} ({})", g.get("why").and_then(Value::as_str).unwrap_or(""), g.get("cites").map(Value::to_string).unwrap_or_default());
        }
    }
}

/// The harnesses a run drives: `VM_E2E_KINDS` (`claude,codex`), else all ten.
fn asked_kinds() -> Vec<&'static str> {
    let asked = std::env::var("VM_E2E_KINDS").unwrap_or_default();
    let wanted: Vec<&str> = asked.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    vm_e2e::KINDS.into_iter().filter(|k| wanted.is_empty() || wanted.contains(k)).collect()
}

/// Whether this run drives `kind`'s stand-in, an image of it ROOT names (`VM_E2E_IMAGE_<KIND>`),
/// rather than its vendor's own CLI.
fn stand_in(kind: &str) -> bool {
    std::env::var_os(format!("VM_E2E_IMAGE_{}", kind.to_ascii_uppercase())).is_some()
}

/// Why a drive of `kind` from `image` is held back, or none. A run of the vendor's own CLI (no
/// stand-in named for `kind`), from kiln's Agentfile or from a person's own made from it, takes ROOT's
/// task alone, each of [`vm_e2e::TASK_WORDS`] (`given`), as `vm_e2e::untasked` holds kiln's own image
/// to it: never the stand-in's words sent to a model.
fn untasked_by(kind: &str, image: &str, stand_in: bool, given: impl Fn(&str) -> bool) -> Option<String> {
    if stand_in {
        return None;
    }
    let missing: Vec<&str> = vm_e2e::TASK_WORDS.iter().copied().filter(|v| !given(v)).collect();
    (!missing.is_empty()).then(|| format!("SKIP: {kind} from {image} is the vendor's own CLI, which takes ROOT's task alone, and {} isn't given", missing.join(", ")))
}

/// [`untasked_by`], from this run's environment.
fn untasked(kind: &str, image: &str) -> Option<String> {
    untasked_by(kind, image, stand_in(kind), |v| std::env::var_os(v).is_some())
}

/// What a drive held back comes to: said and skipped, or with `VM_E2E=1` a failure.
fn held_back(why: String, failed: &mut Vec<String>) {
    if vm_e2e::isolation_required() {
        failed.push(format!("{why} (VM_E2E=1 requires a real task)"));
    } else {
        eprintln!("{why}");
    }
}

// =================================================================================================
// The gap table, held to kiln and to its sources, and the rules a run holds itself to. Each guard's
// mutation is named beside it.
// =================================================================================================
mod checks {
    use serde_json::Value;

    const FLAGS: [&str; 5] = ["linux_build", "mcp", "mcp_in_microvm", "resume", "ui_pick_up"];

    /// The repository's root, where gaps.json's paths start.
    fn root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Ok when `cite` is a URL; a file of the repository at `root` holding the text it names
    /// (`path#text`), as kiln's code is cited, by what it holds, never by a line, which moves; or a
    /// recorded file outside kiln's code (`docs/research/...`) that has the line it names
    /// (`path:line`), or that is there (`path`).
    fn cited_in(root: &std::path::Path, cite: &str) -> Result<(), String> {
        if cite.starts_with("https://") {
            return Ok(());
        }
        if let Some((path, needle)) = cite.split_once('#') {
            let text = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{cite}: {e}"))?;
            return if !needle.is_empty() && text.contains(needle) { Ok(()) } else { Err(format!("{cite}: {path} doesn't hold {needle:?}")) };
        }
        let (path, line) = match cite.rsplit_once(':') {
            Some((p, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => (p, l.parse::<usize>().ok()),
            _ => (cite, None),
        };
        if line.is_some() && path.starts_with("crates/") {
            return Err(format!("{cite}: kiln's code is cited by the text it holds (path#text), not by a line, which moves"));
        }
        let text = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{cite}: {e}"))?;
        match line {
            Some(n) if n == 0 || n > text.lines().count() => Err(format!("{cite}: {path} has {} lines", text.lines().count())),
            _ => Ok(()),
        }
    }

    fn cited(cite: &str) -> Result<(), String> {
        cited_in(&root(), cite)
    }

    /// The strings of `v`'s array `key`.
    fn strings<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
        v.get(key).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect()
    }

    /// The cite rule itself, over a scratch repository: kiln's code by the text it holds, a recorded
    /// file by its line, a URL as it is; a text that isn't there, a line past the end, a file that
    /// isn't there and a line of code each refused. Taking out the code-line rule passes the last.
    #[test]
    fn s35_a_cite_is_held_to_what_it_names() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("crates/k/src"))?;
        std::fs::create_dir_all(dir.path().join("docs/research"))?;
        std::fs::write(dir.path().join("crates/k/src/a.rs"), "fn one() {}\nfn two() {}\n")?;
        std::fs::write(dir.path().join("docs/research/run.log"), "a\nb\nc\n")?;
        let at = |c: &str| cited_in(dir.path(), c);
        assert_eq!(at("https://example.org/doc#part"), Ok(()));
        assert_eq!(at("crates/k/src/a.rs#fn two()"), Ok(()));
        assert_eq!(at("docs/research/run.log:3"), Ok(()));
        assert_eq!(at("docs/research/run.log"), Ok(()));
        assert!(at("crates/k/src/a.rs#fn three()").is_err_and(|e| e.contains("doesn't hold")));
        assert!(at("crates/k/src/a.rs#").is_err(), "an empty text holds nothing");
        assert!(at("docs/research/run.log:4").is_err_and(|e| e.contains("has 3 lines")));
        assert!(at("docs/research/run.log:0").is_err());
        assert!(at("docs/research/gone.log").is_err());
        assert!(at("crates/k/src/a.rs:1").is_err_and(|e| e.contains("not by a line")), "a line of kiln's code moves");
        Ok(())
    }

    /// Every harness has its row; each flag is a boolean (null where a fact isn't recorded yet, but
    /// never `linux_build`, `mcp` or `ui_pick_up`, which kiln's own code says); each flag false or null
    /// has a gap naming it, with why and sources that hold; each caveat says what and cites what holds
    /// it; the row's reason rests on sources, its pin first. Taking out the per-flag rule passes a row
    /// with a false flag and no gap; taking out `cited` passes a source that isn't there.
    #[test]
    fn s35_every_harness_has_its_row_and_each_gap_its_cited_reason() -> anyhow::Result<()> {
        let gaps = super::gaps()?;
        for kind in crate::vm_e2e::KINDS {
            let row = gaps.get(kind).ok_or_else(|| anyhow::anyhow!("gaps.json has no row for {kind}"))?;
            let listed: Vec<(&str, &Value)> = row.get("gaps").and_then(Value::as_array).into_iter().flatten().map(|g| (g.get("what").and_then(Value::as_str).unwrap_or(""), g)).collect();
            for flag in FLAGS {
                let v = row.get(flag).ok_or_else(|| anyhow::anyhow!("{kind} says nothing of {flag}"))?;
                let known = ["linux_build", "mcp", "ui_pick_up"].contains(&flag);
                anyhow::ensure!(v.is_boolean() || (v.is_null() && !known), "{kind}'s {flag} is {v}");
                let gap = listed.iter().find(|(w, _)| *w == flag);
                anyhow::ensure!(v.as_bool() == Some(true) || gap.is_some(), "{kind}'s {flag} is {v} with no gap saying why");
                anyhow::ensure!(v.as_bool() != Some(true) || gap.is_none(), "{kind}'s {flag} holds, yet a gap says it doesn't");
            }
            for (what, g) in &listed {
                anyhow::ensure!(FLAGS.contains(what), "{kind} lists a gap of {what}, which no flag is");
                anyhow::ensure!(g.get("why").and_then(Value::as_str).is_some_and(|w| !w.trim().is_empty()), "{kind}'s {what} gap says no why");
                let cites = strings(g, "cites");
                anyhow::ensure!(!cites.is_empty(), "{kind}'s {what} gap cites nothing");
                for c in cites {
                    cited(c).map_err(|e| anyhow::anyhow!("{kind}'s {what} gap: {e}"))?;
                }
            }
            for c in row.get("caveats").and_then(Value::as_array).into_iter().flatten() {
                let note = c.get("note").and_then(Value::as_str).unwrap_or_default();
                anyhow::ensure!(!note.trim().is_empty(), "{kind} has a caveat that says nothing");
                let cites = strings(c, "cites");
                anyhow::ensure!(!cites.is_empty(), "{kind}'s caveat {note:?} cites nothing");
                for s in cites {
                    cited(s).map_err(|e| anyhow::anyhow!("{kind}'s caveat {note:?}: {e}"))?;
                }
            }
            anyhow::ensure!(row.get("reason").and_then(Value::as_str).is_some_and(|r| !r.trim().is_empty()), "{kind} says no reason");
            let sources = strings(row, "sources");
            let pin = format!(".kiln/images/harness-{kind}/pin.json");
            anyhow::ensure!(sources.first() == Some(&pin.as_str()), "{kind}'s reason rests on its pin, {pin}, first: {sources:?}");
            for s in sources {
                cited(s).map_err(|e| anyhow::anyhow!("{kind}'s sources: {e}"))?;
            }
        }
        Ok(())
    }

    /// The flags kiln's code decides agree with it: a Linux build where kiln ships an Agentfile for the
    /// harness, and kiln's MCP server on the host for every harness but Pi, whose RPC mode takes none
    /// (`Protocol::PiRpc`). Flipping Pi's `mcp`, or any `linux_build`, fails it.
    #[test]
    fn s35_the_rows_agree_with_kilns_code() -> anyhow::Result<()> {
        use kiln::harness::kinds::{Kind, Protocol};
        let gaps = super::gaps()?;
        for kind in crate::vm_e2e::KINDS {
            let k = Kind::get(kind).ok_or_else(|| anyhow::anyhow!("kiln has no harness {kind}"))?;
            let shipped = kiln::agentfiles::builtin::ALL.iter().any(|b| b.harness == kind);
            anyhow::ensure!(gaps.pointer(&format!("/{kind}/linux_build")) == Some(&Value::Bool(shipped)), "{kind}: linux_build says {:?}, kiln ships {shipped}", gaps.pointer(&format!("/{kind}/linux_build")));
            let gives = k.protocol != Protocol::PiRpc;
            anyhow::ensure!(gaps.pointer(&format!("/{kind}/mcp")) == Some(&Value::Bool(gives)), "{kind}: mcp says {:?}, kiln gives it {gives}", gaps.pointer(&format!("/{kind}/mcp")));
            anyhow::ensure!(gives || gaps.pointer(&format!("/{kind}/mcp_in_microvm")) == Some(&Value::Bool(false)), "{kind}: no MCP on the host, none in a microVM");
        }
        Ok(())
    }

    /// A run of a vendor's own CLI waits on every one of ROOT's task words, naming those it lacks; a
    /// stand-in's run takes the stand-in's. Taking out the stand-in rule holds a stand-in back; taking
    /// out the words rule lets the stand-in's words reach a vendor's CLI.
    #[test]
    fn s35_a_vendors_cli_takes_roots_task_alone() {
        let none = |_: &str| false;
        let all = |_: &str| true;
        let held = super::untasked_by("codex", "agentfile:0123", false, none);
        assert!(held.as_deref().is_some_and(|w| crate::vm_e2e::TASK_WORDS.iter().all(|v| w.contains(v))), "{held:?}");
        assert_eq!(super::untasked_by("codex", "agentfile:0123", false, all), None);
        let but_marker = |v: &str| v != "VM_E2E_DONE_MARKER";
        assert!(super::untasked_by("codex", "harness-codex", false, but_marker).is_some_and(|w| w.ends_with("VM_E2E_DONE_MARKER isn't given")));
        assert_eq!(super::untasked_by("codex", "agentfile:0123", true, none), None, "the stand-in takes its own words");
    }
}

// =================================================================================================
// The eight harnesses step 34's file doesn't drive, through kiln on this host (the stand-in behind
// each name): every step passes, and every check of where it ran says "the host".
// =================================================================================================
#[cfg(unix)]
mod host {
    fn on_host(kind: &str) -> anyhow::Result<()> {
        let gaps = super::gaps()?;
        let mcp = super::tried(&gaps, kind, "mcp");
        if !mcp {
            super::say_gaps(&gaps, kind, "mcp");
        }
        crate::vm_e2e::on_host(kind, mcp)
    }

    macro_rules! each {
        ($($kind:ident),*) => { $( #[test] fn $kind() -> anyhow::Result<()> { on_host(stringify!($kind)) } )* };
    }
    each!(gemini, opencode, goose, amp, devin, antigravity, cursor, pi);
}

// =================================================================================================
// The ten in microVMs through kiln's REST API, from kiln's Agentfile and from a person's own.
// =================================================================================================
mod rest {
    use serde_json::{Value, json};

    use crate::vm_e2e::{self, Fixture};

    /// A person's own Agentfile for `kind` (`agentfile:<id>`, as a start names it): the text of
    /// `VM_E2E_OWN_<KIND>` (a file ROOT names), else kiln's own for it, kept as theirs as the
    /// console's "Copy as yours" keeps it. None where the stand-in's image is asked for and no text
    /// is named: kiln's own text runs the vendor's CLI, which a stand-in run never starts.
    pub fn users_own(f: &Fixture, kind: &str) -> anyhow::Result<Option<String>> {
        let named = std::env::var(format!("VM_E2E_OWN_{}", kind.to_ascii_uppercase())).ok();
        let text = match named {
            Some(path) => std::fs::read_to_string(&path)?,
            None if super::stand_in(kind) => return Ok(None),
            None => {
                let listing = f.get("/api/agentfiles")?;
                let builtin = listing.get("builtin").and_then(Value::as_array).into_iter().flatten().find(|b| b.get("harness").and_then(Value::as_str) == Some(kind));
                builtin.and_then(|b| b.get("text").and_then(Value::as_str)).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for {kind}"))?
            }
        };
        let name = format!("{kind}-own-{}", std::process::id());
        let (code, body) = crate::common::http(f.port, "POST", "/api/agentfiles", &f.admin, Some(&json!({"name": name, "text": text})))?;
        anyhow::ensure!(code == 200, "keeping {name}: {code} {body}");
        let id = serde_json::from_str::<Value>(&body)?.pointer("/agentfile/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no id in {body}"))?;
        Ok(Some(format!("agentfile:{id}")))
    }

    /// Each harness in a microVM from kiln's Agentfile and from a person's own, each starting an agent
    /// from inside where kiln gives it its MCP server; every harness is driven and every failure said
    /// at the end, so one run shows where each stands.
    #[test]
    fn s35_every_harness_in_a_microvm_from_kilns_agentfile_and_a_persons_own() -> anyhow::Result<()> {
        let gaps = super::gaps()?;
        let dir = tempfile::tempdir()?;
        let f = vm_e2e::up(dir.path(), "vm", &[])?;
        if !vm_e2e::isolation_ready(&f)? {
            return Ok(());
        }
        let project = vm_e2e::project(dir.path())?;
        let mut failed = Vec::new();
        for kind in super::asked_kinds() {
            if !super::tried(&gaps, kind, "linux_build") {
                super::say_gaps(&gaps, kind, "linux_build");
                continue;
            }
            let mcp = super::tried(&gaps, kind, "mcp_in_microvm");
            if !mcp {
                super::say_gaps(&gaps, kind, "mcp_in_microvm");
            }
            // kiln's own image: `in_microvm` holds it to ROOT's task itself.
            let kilns = vm_e2e::builtin_image(kind);
            if let Err(e) = vm_e2e::in_microvm(&f, kind, &kilns, &project, mcp, mcp) {
                failed.push(format!("{kind} from {kilns}: {e:#}"));
            }
            // A person's own: held to ROOT's task here, as an `agentfile:<id>` is a stand-in's to it.
            if let Some(why) = super::untasked(kind, "a person's own Agentfile") {
                super::held_back(why, &mut failed);
                continue;
            }
            match users_own(&f, kind) {
                Ok(Some(own)) => {
                    if let Err(e) = vm_e2e::in_microvm(&f, kind, &own, &project, mcp, mcp) {
                        failed.push(format!("{kind} from a person's own ({own}): {e:#}"));
                    }
                }
                Ok(None) => eprintln!("SKIP: {kind} from a person's own Agentfile: the stand-in's image is asked for (VM_E2E_IMAGE_{}) and no text of a person's is named (VM_E2E_OWN_{})", kind.to_ascii_uppercase(), kind.to_ascii_uppercase()),
                Err(e) => failed.push(format!("{kind}: keeping a person's own Agentfile: {e:#}")),
            }
        }
        anyhow::ensure!(failed.is_empty(), "{} of the drives failed:\n{}", failed.len(), failed.join("\n\n"));
        Ok(())
    }

    /// Two Antigravity agents of one person on one machine, side by side, each in a microVM of its own
    /// from the same Agentfile. agy takes kiln's MCP from its one config in the person's volume, the same
    /// entry for both, and the lane names the session (gaps.json's antigravity caveat; Shards D254): each
    /// must reach kiln's MCP as itself. The stand-in says whom kiln's answer to its `initialize` named
    /// ([`vm_e2e::check_named_itself`]); a real agy's calls of kiln's tools are in the runner's log under
    /// its own session ([`vm_e2e::mcp_calls_over_lane`]).
    #[test]
    fn s35_two_antigravity_agents_of_one_person_each_reach_kilns_mcp_as_themselves() -> anyhow::Result<()> {
        const KIND: &str = "antigravity";
        if !super::asked_kinds().contains(&KIND) {
            eprintln!("SKIP: {KIND} isn't among VM_E2E_KINDS");
            return Ok(());
        }
        let gaps = super::gaps()?;
        for flag in ["linux_build", "mcp_in_microvm"] {
            if !super::tried(&gaps, KIND, flag) {
                super::say_gaps(&gaps, KIND, flag);
                return Ok(());
            }
        }
        let asked = vm_e2e::builtin_image(KIND);
        if let Some(why) = super::untasked(KIND, &asked) {
            let mut failed = Vec::new();
            super::held_back(why, &mut failed);
            anyhow::ensure!(failed.is_empty(), "{}", failed.join("\n"));
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let f = vm_e2e::up(dir.path(), "vm", &[])?;
        if !vm_e2e::isolation_ready(&f)? {
            return Ok(());
        }
        let project = vm_e2e::project(dir.path())?;
        let (_stand_in, image) = vm_e2e::boots(&f, KIND, &asked)?;
        let marker = std::env::var("VM_E2E_DONE_MARKER").unwrap_or_else(|_| vm_e2e::STUB_DONE.to_owned());
        // Both started, as one person (the fixture's token) on one runner, before either is waited on.
        let (mut agents, mut failed) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            let probe = vm_e2e::probe_name();
            let prompt = vm_e2e::task("VM_E2E_PROMPT", KIND, &probe, vm_e2e::stub_prompt(KIND, true, false, &probe));
            match vm_e2e::open(&f, &json!({"kind": KIND, "cwd": project, "prompt": prompt, "isolation": {"kind": "microvm", "image": image}})) {
                Ok(id) => agents.push((id, probe, vm_e2e::Pace::default())),
                Err(e) => failed.push(format!("{KIND}: starting one from {image}: {e:#}")),
            }
        }
        let mut vms = Vec::new();
        for (id, probe, pace) in &mut agents {
            let what = format!("{KIND} {id} from {image}");
            let first = pace.until(&f, id, vm_e2e::Asked::Opening, &format!("{what}: its first turn"), |i, e| vm_e2e::status(i) == "idle" && vm_e2e::said_marker(&vm_e2e::said(e), &marker));
            let (info, events) = match first {
                Ok(seen) => seen,
                Err(e) => {
                    failed.push(format!("{e:#}"));
                    continue;
                }
            };
            let lines = vm_e2e::said(&events);
            let mut checks = vec![vm_e2e::mcp_round_trip(&events, &lines).map(|_| ()), vm_e2e::written_and_removed(&project, probe)];
            // The runner logs each tool's call over a lane (`reach::mcp`), and the stand-in's round trip calls
            // none: it says whom kiln's answer named instead.
            if super::stand_in(KIND) {
                checks.push(vm_e2e::check_named_itself(&lines, id));
            } else {
                match f.runner_log_whole() {
                    Ok(log) if vm_e2e::mcp_calls_over_lane(&log, id) > 0 => {}
                    Ok(_) => checks.push(Err(format!("its runner's log says no call of kiln's MCP over its lane ({:?}, session={id})", vm_e2e::MCP_LANE_SAID))),
                    Err(e) => checks.push(Err(format!("{e:#}"))),
                }
            }
            failed.extend(checks.into_iter().filter_map(Result::err).map(|e| format!("{what}: {e}")));
            vms.push(info.pointer("/runs/vm").cloned());
        }
        // Each the one member of a microVM of its own, which is how its lane tells it from the other.
        match vms.as_slice() {
            [Some(a), Some(b)] if a != b => {}
            [a, b] => failed.push(format!("{KIND}: the two agents' microVMs as kiln names them (runs.vm) aren't two of their own: {a:?}, {b:?}")),
            // Fewer than two first turns ended, each said above.
            _ => {}
        }
        // Each ended with any agent a real task's first turn started (the README's words ask for one).
        for (id, _, pace) in &mut agents {
            let ended = super::mcp::call(&f, super::mcp::Via::Tools, "agents.stop", json!({"agent": id, "children": true})).and_then(|_| pace.unlisted(&f, id, &format!("{KIND}: {id} still listed after agents.stop")));
            if let Err(e) = ended {
                failed.push(format!("{KIND}: ending {id}: {e:#}"));
            }
        }
        anyhow::ensure!(failed.is_empty(), "{} of the checks failed:\n{}", failed.len(), failed.join("\n\n"));
        Ok(())
    }
}
