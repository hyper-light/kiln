//! Real Codex and Claude Code sessions, each in a Shards microVM of its own, driven by kiln (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 34), over kiln's public surfaces: its REST API, its MCP server
//! and its audit trail. The drive and its checks are `vm_e2e`, which step 35's every-harness runs
//! build on too.
//!
//! - `checks`: the acceptance's checks, against samples (any OS).
//! - `waits`: the drive's waits, which follow the harness rather than a clock, against samples of
//!   kiln's word on a session (any OS).
//! - `stub`: the stand-in harness (`tests/fixtures/vm-stub/harness/stub`) speaking each harness's interface,
//!   driven as a process (Unix: it's a Python script, as `fake-claude` is).
//! - `host`: Claude Code's and Codex's interfaces driven through kiln on the host, the stand-in behind
//!   each: start, steer, interrupt, stop, resume, a file written in the project, the MCP round trip,
//!   an agent started from where it runs. It shows the drive works against kiln, and that each check
//!   of where a harness ran says "the host" here, as it must (Unix).
//! - `step34`: the same drive with each harness in a microVM of its own. It skips, in the runner's own
//!   words, until the runner says it starts microVMs (kiln plan steps 16 and 17); `VM_E2E=1` makes
//!   that a failure. ROOT runs it on each host OS: `tests/vm_e2e/README.md`.
//! - `step35` (`vm_e2e/step35.rs`, `vm_e2e/mcp.rs`): every harness of steps 19 to 28 on the same drive,
//!   through kiln's REST API and its MCP tools, from kiln's Agentfile and from a person's own, each gap
//!   in `vm_e2e/gaps.json` with its cited reason (kiln plan step 35).
//! - `narrowed` (`vm_e2e/narrowed.rs`): a Goose session of kiln's own Agentfile in a microVM, the
//!   stand-in in Goose's place, reaching past it only its person's provider's hosts, and saying so; one
//!   whose home can't be read keeps every destination, saying why (kiln plan step 17 part 6).
//!
//! No real harness and no model runs here unless ROOT points these at one (`VM_E2E_*`).
//!
//!   cargo test -p kiln --test vm_harnesses

mod common;
mod vm_e2e;
#[path = "vm_e2e/step35.rs"]
mod step35;
#[path = "vm_e2e/narrowed.rs"]
mod narrowed;

// =================================================================================================
// The checks, against samples. Each guard has its red beside its green; the comment names the
// mutation that must fail it.
// =================================================================================================
mod checks {
    use crate::vm_e2e::*;
    use serde_json::json;

    fn link(name: &str, client: Option<&str>, server: Option<&str>) -> LinkSuites {
        LinkSuites { name: name.into(), client: client.map(str::to_owned), server: server.map(str::to_owned), lanes: None }
    }

    /// Every link the plan names, both ends at `suite`.
    fn every_link(suite: &str) -> Vec<LinkSuites> {
        REQUIRED_LINKS.iter().map(|n| link(n, Some(suite), Some(suite))).collect()
    }

    #[test]
    fn s34_every_link_negotiates_aes_256_gcm_at_both_ends() {
        assert_eq!(check_link(&link("agent-control", Some(REQUIRED_SUITE), Some(REQUIRED_SUITE))), Ok(()));
        // Either end on ChaCha20-Poly1305 or AES-128-GCM fails, by its end (accepting any TLS 1.3
        // suite, or checking one end, passes these).
        for bad in ["TLS13_CHACHA20_POLY1305_SHA256", "TLS13_AES_128_GCM_SHA256"] {
            assert!(check_link(&link("kiln-hex", Some(bad), Some(REQUIRED_SUITE))).is_err_and(|e| e.contains("client end") && e.contains(bad)));
            assert!(check_link(&link("kiln-hex", Some(REQUIRED_SUITE), Some(bad))).is_err_and(|e| e.contains("server end") && e.contains(bad)));
        }
        // An end that said nothing fails.
        assert!(check_link(&link("mcp", Some(REQUIRED_SUITE), None)).is_err());
        // Every link the plan names, read at both ends, passes; one bad end among them fails, and a
        // link read besides them is held to the suite too.
        assert_eq!(check_all_links(&every_link(REQUIRED_SUITE)), Ok(()));
        let mut one_bad = every_link(REQUIRED_SUITE);
        if let Some(l) = one_bad.last_mut() {
            l.server = Some("TLS13_AES_128_GCM_SHA256".into());
        }
        assert!(check_all_links(&one_bad).is_err());
        let mut more = every_link(REQUIRED_SUITE);
        more.push(link("egress", Some(REQUIRED_SUITE), Some("TLS13_CHACHA20_POLY1305_SHA256")));
        assert!(check_all_links(&more).is_err_and(|e| e.contains("egress")));
    }

    #[test]
    fn s34_a_run_that_reads_fewer_links_than_the_plan_names_fails() {
        // None read, or any one of the plan's links left out (checking only the links read passes
        // these).
        assert!(check_all_links(&[]).is_err());
        for left_out in REQUIRED_LINKS {
            let read: Vec<LinkSuites> = every_link(REQUIRED_SUITE).into_iter().filter(|l| l.name != left_out).collect();
            assert!(check_all_links(&read).is_err_and(|e| e.contains(left_out)), "{left_out} left out");
        }
    }

    #[test]
    fn s34_links_to_read_are_named_end_by_end() {
        let specs = link_specs(r#"[{"name": "agent-control", "client": {"url": "/api/a/{session}", "pointer": "/suite"}, "server": {"url": "/api/b", "pointer": "/peers/0/secured/suite"}}]"#);
        assert_eq!(specs, Ok(vec![LinkSpec { name: "agent-control".into(), client: End { url: "/api/a/{session}".into(), pointer: "/suite".into(), pick: None }, server: End { url: "/api/b".into(), pointer: "/peers/0/secured/suite".into(), pick: None }, lanes: None }]));
        assert!(link_specs(r#"[{"name": "half", "client": {"url": "/x", "pointer": "/s"}}]"#).is_err(), "a link with one end");
        assert!(link_specs(r#"[{"client": {"url": "/x", "pointer": "/s"}, "server": {"url": "/y", "pointer": "/s"}}]"#).is_err(), "a link of no name, which no required link can be");
        assert!(link_specs(r#"{"name": "not a list"}"#).is_err());
        assert!(link_specs("not json").is_err());
        let body = json!({"peers": [{"secured": {"suite": REQUIRED_SUITE, "group": "secp384r1MLKEM1024"}}]});
        assert_eq!(suite_at(&body, "/peers/0/secured/suite").as_deref(), Some(REQUIRED_SUITE));
        assert_eq!(suite_at(&body, "/peers/1/secured/suite"), None);
        // A link's lanes, where its report lists them (a lanes reader of no pointer, or a list of
        // other than names, is none).
        let carried = link_specs(r#"[{"name": "agent-control", "client": {"url": "/a", "pointer": "/s"}, "server": {"url": "/b", "pointer": "/s"}, "lanes": {"url": "/api/harness/sessions/{session}", "pointer": "/links/0/lanes"}}]"#);
        assert_eq!(carried.map(|s| s.into_iter().map(|l| l.lanes).collect::<Vec<_>>()), Ok(vec![Some(End { url: "/api/harness/sessions/{session}".into(), pointer: "/links/0/lanes".into(), pick: None })]));
        // A runner's link in kiln's list of runners (`GET /api/agents`, step 34's plan step 2): picked by
        // its name, then read (a pick of no field, or of two, is refused; a list with no such runner reads
        // nothing).
        let runner = link_specs(r#"[{"name": "server-runner", "client": {"url": "/api/agents", "pick": {"name": "{machine}"}, "pointer": "/link/runner/suite"}, "server": {"url": "/api/agents", "pick": {"name": "{machine}"}, "pointer": "/link/server/suite"}}]"#);
        assert_eq!(runner.map(|s| s.into_iter().map(|l| l.client.pick).collect::<Vec<_>>()), Ok(vec![Some(("name".to_owned(), "{machine}".to_owned()))]));
        assert!(link_specs(r#"[{"name": "x", "client": {"url": "/a", "pointer": "/s", "pick": {}}, "server": {"url": "/b", "pointer": "/s"}}]"#).is_err());
        assert!(link_specs(r#"[{"name": "x", "client": {"url": "/a", "pointer": "/s", "pick": {"name": "a", "id": "b"}}, "server": {"url": "/b", "pointer": "/s"}}]"#).is_err());
        let agents = json!([{"name": "other", "link": {"runner": {"suite": "TLS13_CHACHA20_POLY1305_SHA256"}}}, {"name": "box", "link": {"runner": {"suite": REQUIRED_SUITE}, "server": {"suite": REQUIRED_SUITE}}}]);
        let pick = ("name".to_owned(), "box".to_owned());
        assert_eq!(picked(&agents, Some(&pick)).and_then(|a| suite_at(a, "/link/runner/suite")).as_deref(), Some(REQUIRED_SUITE));
        assert_eq!(picked(&agents, Some(&("name".to_owned(), "gone".to_owned()))), None);
        assert_eq!(picked(&agents, None), Some(&agents));
        assert!(link_specs(r#"[{"name": "x", "client": {"url": "/a", "pointer": "/s"}, "server": {"url": "/b", "pointer": "/s"}, "lanes": {"url": "/c"}}]"#).is_err());
        let report = json!({"links": [{"lanes": ["control", "mcp"]}, {"lanes": ["control", 7]}]});
        assert_eq!(lanes_at(&report, "/links/0/lanes"), Some(vec!["control".to_owned(), "mcp".to_owned()]));
        assert_eq!(lanes_at(&report, "/links/1/lanes"), None);
    }

    #[test]
    fn s34_kilns_mcp_rides_the_agent_control_links_own_lane() {
        // Shards D254: the agent-control link's report names a member's mcp lane, as Shards names its
        // lanes (`control::Granted::name`; dropping the lane's test from the check, or taking a link
        // that listed no lanes, passes a red below).
        let (carrier, _) = MCP_LANE;
        let with = |name: &str, lanes: Option<&[&str]>| vec![LinkSuites { name: name.into(), client: None, server: None, lanes: lanes.map(|l| l.iter().map(|n| (*n).to_owned()).collect()) }];
        assert_eq!(check_mcp_lane(&with(carrier, Some(&["control", "stdin:0", "stdout:0", "stderr:0", "exec", "mcp:0"]))), Ok(()));
        assert!(check_mcp_lane(&with(carrier, Some(&["control", "stdin:0", "exec"]))).is_err_and(|e| e.contains("none is a mcp:")));
        assert!(check_mcp_lane(&with(carrier, Some(&["control", "mcp"]))).is_err(), "no lane Shards names");
        assert!(check_mcp_lane(&with(carrier, None)).is_err_and(|e| e.contains("no lanes")));
        assert!(check_mcp_lane(&with("mcp", Some(&["mcp:0"]))).is_err_and(|e| e.contains(carrier)), "a lane on a link of its own isn't agent control's");
        assert!(check_mcp_lane(&[]).is_err());
    }

    #[test]
    fn s34_the_runner_says_each_mcp_call_came_over_the_lane() {
        // kiln's runner logs each call of its MCP it takes over a session's lane (`reach::mcp::serve_over`),
        // as text or as JSON; another session's, another event's, or an id the line only starts with is
        // none (dropping the session from the match passes a red below).
        let s = "01a1d5280f617648b8b9dbc21c09f73c";
        let text = format!("2026-11-13T14:00:00.000000Z  INFO {MCP_LANE_SAID} session={s} tool=\"runners\"");
        let json = format!(r#"{{"timestamp":"2026-11-13T14:00:00Z","level":"INFO","fields":{{"message":"{MCP_LANE_SAID}","session":"{s}","tool":"runners"}}}}"#);
        let log = format!("{text}\n2026-11-13T14:00:01.000000Z  INFO another event session={s}\n{json}\n");
        assert_eq!(mcp_calls_over_lane(&log, s), 2);
        assert_eq!(mcp_calls_over_lane(&log, "01a1d5280f617648b8b9dbc21c09f73d"), 0, "another session's");
        assert_eq!(mcp_calls_over_lane(&format!("{MCP_LANE_SAID} session={s}ff\n"), s), 0, "an id it only starts with");
        assert_eq!(mcp_calls_over_lane("", s), 0);
    }

    #[test]
    fn s34_kiln_says_the_session_holds_no_host_process_and_started_it_in_microvms_alone() {
        // kiln plan step 17's `runs` and `starts` on the session (dropping the host_pid test, or
        // checking only the last start, passes a red below).
        let at = |runs: serde_json::Value| json!({"runs": runs, "starts": [{"at": 1, "runs": {"in": "microvm", "vm": "a", "host_pid": null}}, {"at": 2, "runs": {"in": "microvm", "vm": "b", "host_pid": null}}]});
        let vm = at(json!({"in": "microvm", "vm": "b", "image": "harness-claude@sha256:00", "host_pid": null}));
        assert_eq!(check_no_host_pid(&vm), Ok(()));
        assert_eq!(check_starts(&vm), Ok(()));
        assert!(check_no_host_pid(&at(json!({"in": "microvm", "vm": "b", "host_pid": 4242}))).is_err_and(|e| e.contains("4242")));
        assert!(check_no_host_pid(&at(json!({"in": "microvm", "vm": "b"}))).is_err(), "host_pid not said");
        assert!(check_no_host_pid(&at(json!({"in": "host", "pid": 4242}))).is_err_and(|e| e.contains("host")));
        assert!(check_no_host_pid(&json!({})).is_err());
        let mut on_host_once = vm;
        if let Some(first) = on_host_once.pointer_mut("/starts/0/runs") {
            *first = json!({"in": "host", "pid": 4242});
        }
        assert!(check_starts(&on_host_once).is_err_and(|e| e.contains("4242")), "a start on the host before a microVM's");
        assert!(check_starts(&json!({"starts": []})).is_err());
        assert!(check_starts(&json!({})).is_err());
    }

    #[test]
    fn s34_no_grant_of_an_image_reaches_kiln_past_its_mcp_lane() -> anyhow::Result<()> {
        // kiln's MCP rides the agent-control link (Shards D254): kiln's own images for Claude Code
        // and Codex grant no way to kiln, at its own default port (`kiln::config`), and each way an
        // image could is found (dropping the destination's, the any-address range's or the MCP
        // server's guard fails its line below).
        let port = kiln_default_port().ok_or_else(|| anyhow::anyhow!("kiln's default listens at no port"))?;
        let images = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln/images");
        for kind in ["claude", "codex"] {
            let text = std::fs::read_to_string(images.join(format!("harness-{kind}/Agentfile")))?;
            assert_eq!(grants_to_kiln(&text, &[port]), Ok(vec![]), "harness-{kind}");
        }
        let claude = std::fs::read_to_string(images.join("harness-claude/Agentfile"))?;
        let with = |line: &str| format!("{claude}\n{line}\n");
        let (below, above) = (port.saturating_sub(1), port.saturating_add(1));
        for line in [
            format!("NETWORK --egress={MACHINE}:{port} kiln FOR claude"),
            format!("NETWORK --egress={port}/tcp kiln FOR claude"),
            format!("NETWORK --expose={below}-{above} kiln FOR claude"),
            format!("MCP kiln FROM http://{MACHINE}:{port}/api/mcp FOR claude"),
        ] {
            assert_eq!(grants_to_kiln(&with(&line), &[port]).map(|found| found.len()), Ok(1), "{line}");
        }
        // Not kiln: the machine at another port, a host past it at kiln's port, or UDP.
        for line in [format!("NETWORK --egress={MACHINE}:{above} db FOR claude"), format!("NETWORK --egress=api.example.com:{port} other FOR claude"), format!("NETWORK --egress={port}/udp quic FOR claude")] {
            assert_eq!(grants_to_kiln(&with(&line), &[port]), Ok(vec![]), "{line}");
        }
        Ok(())
    }

    #[test]
    fn s34_the_stand_in_is_an_image_of_its_text_alone() -> anyhow::Result<()> {
        // kiln plan step 17 builds a person's agentfile:<id> from its text alone (no context, no build
        // arguments): the stand-in's harness is named as kind's, which kiln starts by that name, comes
        // from kiln's repository at a commit to where its harness.json runs it, and nothing of it
        // reaches past its microVM (a COPY, a name of its own or any grant fails here).
        use shards_agentfile::agentfile::{Directive, Source, source_of};
        let (dir, at) = STUB_HARNESS;
        let commit = "0123456789abcdef0123456789abcdef01234567";
        for kind in ["claude", "codex"] {
            let text = stub_agentfile(kind, kind, commit, &kiln_base()?, &kiln_repository()?);
            let all = directives(&text).map_err(|e| anyhow::anyhow!(e))?;
            let harnesses: Vec<_> = all.iter().filter_map(|d| if let Directive::Harness(h) = d { Some(h) } else { None }).collect();
            let [h] = harnesses.as_slice() else { anyhow::bail!("one HARNESS, not {}: {text}", harnesses.len()) };
            assert_eq!(h.name, kind.as_bytes(), "{text}");
            assert_eq!(h.to.as_deref(), Some(at.as_bytes()), "{text}");
            let Ok(Source::Git(g)) = source_of(&h.source) else { anyhow::bail!("not a Git source: {text}") };
            assert_eq!((g.reference.as_slice(), g.subdir.as_slice()), (commit.as_bytes(), dir.as_bytes()), "{text}");
            assert!(!all.iter().any(|d| matches!(d, Directive::Network(_) | Directive::Expose(_) | Directive::Connect(_) | Directive::Mcp(_))), "a grant past the microVM: {text}");
            assert!(text.lines().any(|l| l == format!("ENV VM_STUB_PROTOCOL={kind}")) && !text.contains("COPY "), "{text}");
        }
        let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stub_path().with_file_name("harness.json"))?)?;
        assert_eq!(manifest.get("entrypoint").and_then(serde_json::Value::as_str), Some(format!("python3 {at}/stub").as_str()));
        Ok(())
    }

    fn process(pid: u32, ppid: u32, argv: &[&str]) -> HostProc {
        HostProc { pid, ppid, argv: argv.iter().map(|a| (*a).to_owned()).collect() }
    }

    #[test]
    fn s34_harness_processes_in_the_sessions_trees_are_found() {
        let programs = ["claude", "codex", "agent", "amp-acp", "stub"];
        let host = [
            process(1, 0, &["/sbin/launchd"]),
            process(100, 1, &["/usr/local/bin/kiln", "agent", "--server", "http://127.0.0.1:7070"]),
            // The boundary under the runner: a microVM's processes, not a harness.
            process(101, 100, &["/home/ada/.cache/shards/helpers/x/shards-vm", "--grants", "3"]),
            process(102, 101, &["shards-net"]),
            // A person's own Claude Code, beside the runner: not the session's.
            process(200, 1, &["/usr/bin/claude", "--resume", "x"]),
        ];
        assert!(harnesses_on_host(&host, &[100], &programs).is_empty(), "the boundary alone under the runner");
        // A harness the runner started on the host, its own child or deeper, by its program, or by
        // the file its interpreter runs (dropping the interpreter rule misses the stand-in and
        // Codex's npm launcher; dropping the tree walk misses the grandchild).
        let mut ran = host.to_vec();
        ran.push(process(110, 100, &["/opt/homebrew/Cellar/python@3.13/Frameworks/Python", "/tmp/t/bin/claude", "-p"]));
        ran.push(process(111, 100, &["sh", "-c", "x"]));
        ran.push(process(112, 111, &["node", "--no-warnings", "/usr/lib/node_modules/@openai/codex/bin/codex.js", "app-server"]));
        ran.push(process(113, 100, &["C:\\Users\\ada\\bin\\amp-acp.exe"]));
        ran.push(process(114, 100, &["python3.13", "/opt/vm-stub/stub"]));
        let found: Vec<u32> = harnesses_on_host(&ran, &[100], &programs).iter().map(|p| p.pid).collect();
        assert_eq!(found, [110, 112, 113, 114]);
        // A daemon the session's microVMs run under that left the runner's tree is a root of its own:
        // a harness under it is found once it's named (looking under the runner alone misses it).
        let mut detached = host.to_vec();
        detached.push(process(300, 1, &["/usr/local/bin/shards", "daemon"]));
        detached.push(process(301, 300, &["/usr/bin/codex", "app-server"]));
        assert!(harnesses_on_host(&detached, &[100], &programs).is_empty());
        let found: Vec<u32> = harnesses_on_host(&detached, &[100, 300], &programs).iter().map(|p| p.pid).collect();
        assert_eq!(found, [301]);
        // A loop in the listing (a pid reused while it was read) ends the walk.
        assert!(harnesses_on_host(&[process(5, 6, &["claude"]), process(6, 5, &["x"])], &[100], &programs).is_empty());
    }

    #[test]
    fn s34_the_roots_looked_under_are_the_runners_and_those_named() {
        assert_eq!(roots_of(Some(100), None), Ok(vec![100]));
        assert_eq!(roots_of(Some(100), Some(" 300, 400 ,")), Ok(vec![100, 300, 400]));
        assert_eq!(roots_of(None, None), Ok(vec![]));
        // A word that names no pid is refused, not passed over (skipping it hides its tree).
        assert!(roots_of(Some(100), Some("300,shards")).is_err_and(|e| e.contains("shards")));
    }

    #[test]
    fn s34_this_hosts_processes_are_listed_with_their_parents() -> anyhow::Result<()> {
        let me = std::process::id();
        let procs = host_processes()?;
        let mine = procs.iter().find(|p| p.pid == me).ok_or_else(|| anyhow::anyhow!("this test's own process ({me}) isn't among the host's {}", procs.len()))?;
        #[cfg(unix)]
        assert_eq!(mine.ppid, std::os::unix::process::parent_id(), "{mine:?}");
        // Its tree is found from its parent, by its program's own name (`vm_harnesses-<hash>`).
        let name = std::env::current_exe()?.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        assert!(harnesses_on_host(&procs, &[mine.ppid], &[name.as_str()]).iter().any(|p| p.pid == me), "{name} under {}: {mine:?}", mine.ppid);
        Ok(())
    }

    #[test]
    fn s34_the_kernel_a_harness_ran_on_is_a_guests() {
        let host_boot = "11111111-2222-3333-4444-555555555555";
        let said = |l: &str| where_said(&[l.to_owned()]);
        let guest = said("WHERE Linux 99999999-2222-3333-4444-555555555555");
        assert_eq!(check_in_guest(guest.as_ref(), "linux", Some(host_boot)), Ok(()));
        assert_eq!(check_in_guest(guest.as_ref(), "macos", None), Ok(()));
        assert_eq!(check_in_guest(guest.as_ref(), "windows", None), Ok(()));
        // The host's own kernel: its boot id (a container's), or another kernel altogether. Dropping
        // the boot id comparison passes the first.
        assert!(check_in_guest(said(&format!("WHERE Linux {host_boot}")).as_ref(), "linux", Some(host_boot)).is_err());
        assert!(check_in_guest(said("WHERE Darwin -").as_ref(), "macos", None).is_err());
        assert!(check_in_guest(said("WHERE Linux -").as_ref(), "linux", Some(host_boot)).is_err(), "no boot id: can't be told apart");
        assert!(check_in_guest(None, "macos", None).is_err());
        assert_eq!(where_said(&["`WHERE Linux abc`".to_owned()]), Some(Where { kernel: "Linux".into(), boot: Some("abc".into()) }));
    }

    #[test]
    fn s34_the_project_is_at_its_host_path_inside_and_out() {
        let p = "/home/ada/src/app";
        for os in ["linux", "macos"] {
            assert_eq!(check_project_path(Some(p), Some(p), p, os), Ok(()));
            assert!(check_project_path(Some("/other"), Some(p), p, os).is_err());
            assert!(check_project_path(Some(p), Some("/workspace"), p, os).is_err(), "copied under another path");
            assert!(check_project_path(Some(p), None, p, os).is_err());
            assert!(check_project_path(None, Some(p), p, os).is_err());
        }
        // On a Windows host its guest sees it in D235's WSL form (comparing the host's own form
        // instead, or dropping the drive's letter, fails these).
        let w = r"C:\Users\ada\src\app";
        assert_eq!(guest_path(w, "windows").as_deref(), Ok("/mnt/c/Users/ada/src/app"));
        assert_eq!(guest_path(r"\\?\D:\work\app\", "windows").as_deref(), Ok("/mnt/d/work/app"));
        assert_eq!(check_project_path(Some(w), Some("/mnt/c/Users/ada/src/app"), w, "windows"), Ok(()));
        assert!(check_project_path(Some(w), Some(w), w, "windows").is_err(), "the host's own form isn't the guest's");
        assert!(check_project_path(Some(w), Some("/mnt/d/Users/ada/src/app"), w, "windows").is_err(), "another drive");
        assert!(guest_path(r"\\server\share\app", "windows").is_err(), "a share is on no drive");
        assert_eq!(guest_path(p, "linux").as_deref(), Ok(p));
        assert_eq!(cwd_said(&["CWD /w".to_owned(), "x".to_owned(), "**CWD /home/ada/src/app**".to_owned()]).as_deref(), Some(p));
    }

    #[test]
    fn s34_a_file_the_harness_wrote_is_read_back_at_the_projects_host_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let name = "vm-e2e-1.txt";
        // Not written, or written with other words (taking any file of that name passes the second).
        assert!(check_written(dir.path(), name).is_err_and(|e| e.contains("isn't at")));
        std::fs::write(dir.path().join(name), "something else\n")?;
        assert!(check_written(dir.path(), name).is_err());
        std::fs::write(dir.path().join(name), format!("{name}\n"))?;
        assert_eq!(check_written(dir.path(), name), Ok(()));
        Ok(())
    }

    #[test]
    fn s34_the_first_turn_asks_for_what_the_checks_read() {
        assert_eq!(stub_prompt("codex", true, false, "f.txt"), "WRITE=f.txt MCP_PING do the task");
        assert_eq!(stub_prompt("codex", true, true, "f.txt"), "WRITE=f.txt MCP_PING SPAWN=codex do the task");
        assert_eq!(stub_prompt("pi", false, true, "f.txt"), "WRITE=f.txt do the task", "no MCP server, so no agent through one");
    }

    #[test]
    fn s34_what_a_harness_said_is_read_line_by_line() {
        // An ACP reply in chunks kiln joins, a tool's output, a person's message (not the harness's).
        let events = [
            json!({"kind": "user", "text": "CWD /not-said"}),
            json!({"kind": "assistant", "text": "CWD /w\nWHERE Linux b\n`MCP_OK 3 runners`\n"}),
            json!({"kind": "tool", "name": "Bash", "tool": "shell", "status": "done", "output": "STUB_DONE\n"}),
        ];
        let lines = said(&events);
        assert_eq!(lines, ["CWD /w", "WHERE Linux b", "MCP_OK 3 runners", "STUB_DONE"]);
        assert!(said_marker(&lines, STUB_DONE));
    }

    #[test]
    fn s34_kilns_mcp_server_is_reached_from_where_the_harness_runs() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(mcp_round_trip(&[], &lines(&["MCP_OK 11 runners"])).is_ok());
        // Nothing listed is no round trip (accepting any MCP_OK passes it); a failure says why.
        assert!(mcp_round_trip(&[], &lines(&["MCP_OK 0 ?"])).is_err());
        assert!(mcp_round_trip(&[], &lines(&["MCP_FAIL no kiln MCP server was given to this harness"])).is_err_and(|e| e.contains("no kiln MCP server")));
        assert!(mcp_round_trip(&[], &lines(&["line 00000"])).is_err());
        // A real harness's call to one of kiln's tools, as kiln shows it; one that failed isn't one.
        let call = |status: &str| json!({"kind": "tool", "name": "mcp__kiln__runners", "tool": "mcp", "server": "kiln", "tool_short": "runners", "status": status});
        assert!(mcp_round_trip(&[call("done")], &[]).is_ok());
        assert!(mcp_round_trip(&[call("error")], &[]).is_err());
        assert!(mcp_round_trip(&[json!({"kind": "tool", "name": "mcp__github__search", "server": "github", "status": "done"})], &[]).is_err());
    }

    #[test]
    fn s34_a_runner_without_microvms_skips_in_its_own_words() {
        let why = "no hypervisor: /dev/kvm is missing";
        let skip = skip_reason("box", Some(false), Some(why));
        assert!(skip.starts_with("SKIP:") && skip.ends_with(why), "{skip}");
        assert!(skip_reason("box", Some(false), None).starts_with("SKIP:"));
        assert!(skip_reason("box", None, None).starts_with("SKIP:"));
        assert!(!skip_reason("box", Some(true), None).starts_with("SKIP:"));
    }

    #[test]
    fn s34_a_vendors_own_cli_takes_roots_task_alone() {
        // kiln's own image for a harness, none of ROOT's words or one short: it doesn't run, saying
        // what's missing (dropping the guard sends the stand-in's words to the vendor's model).
        let none = untasked("codex", "harness-codex", |_| false).unwrap_or_default();
        assert!(none.starts_with("SKIP:") && TASK_WORDS.iter().all(|v| none.contains(v)), "{none}");
        let short = untasked("claude", "harness-claude", |v| v != "VM_E2E_STOPPED_PROMPT").unwrap_or_default();
        assert!(short.ends_with("VM_E2E_STOPPED_PROMPT isn't given"), "{short}");
        // Every word ROOT's, or an image ROOT names (the stand-in's Agentfile): it runs.
        assert_eq!(untasked("claude", "harness-claude", |_| true), None);
        assert_eq!(untasked("codex", "agentfile:01a1d2c1ef09707daafe3fe9b77754e6", |_| false), None);
    }

    #[test]
    fn s35_an_agent_started_from_inside_is_isolated_and_placed() {
        let directory = json!({"runners": [
            {"name": "vm-mac", "facts": {"virt": {"microvm": true}}},
            {"name": "plain", "facts": {"virt": {"microvm": false}}},
            {"name": "new"},
        ]});
        let vms = vm_runners(&directory);
        assert_eq!(vms, ["vm-mac"]);
        let child = |parent: &str, isolation: Option<&str>, target: &str| json!({"id": "c1", "parent": parent, "isolation": isolation.map(|k| json!({"kind": k, "image": "harness-claude"})), "target": target});
        assert_eq!(check_child(&child("p1", Some("microvm"), "agent:vm-mac"), "p1", &vms), Ok(()));
        assert!(check_child(&child("p2", Some("microvm"), "agent:vm-mac"), "p1", &vms).is_err(), "another's agent");
        assert!(check_child(&child("p1", None, "agent:vm-mac"), "p1", &vms).is_err(), "a process on its machine");
        assert!(check_child(&child("p1", Some("microvm"), "agent:plain"), "p1", &vms).is_err(), "on a runner that starts none");
        assert!(check_child(&child("p1", Some("microvm"), "local"), "p1", &vms).is_err(), "on the server");

        // The audit trail's word on its start: agents.start's (a row, or a note of the request it came
        // in), not an open straight through the API's, nor another parent's.
        let parent = "0123456789abcdef0123456789abcdef";
        let c = child(parent, Some("microvm"), "agent:vm-mac");
        let row = |detail: &str| json!({"action": "agent.open", "detail": detail});
        assert_eq!(check_child_start(&[row("1 Claude Code on agent:vm-mac for main (01234567)")], &c, parent), Ok(()));
        assert_eq!(check_child_start(&[json!({"action": "POST /api/mcp", "detail": "a; 1 Claude Code on agent:vm-mac for main (01234567); b"})], &c, parent), Ok(()));
        assert!(check_child_start(&[row("claude on agent:vm-mac")], &c, parent).is_err());
        assert!(check_child_start(&[row("1 Claude Code on agent:vm-mac for other (fedcba98)")], &c, parent).is_err());
        assert!(check_child_start(&[row("1 Claude Code on agent:plain for main (01234567)")], &c, parent).is_err());
    }
}

// =================================================================================================
// The drive's waits, against samples of kiln's word on a session (kiln plan step 35's waits;
// docs/research/s35-every-harness/waits/README.md): they follow the harness, never a clock. Each
// guard's mutation is named beside it.
// =================================================================================================
mod waits {
    use crate::vm_e2e::*;
    use serde_json::{Value, json};
    use std::time::Duration;

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// kiln's word on session `s1`, its harness a process on its machine that started once.
    fn word(status: &str) -> Value {
        json!({"id": "s1", "status": status, "queued": 0, "tasks_running": 0, "runs": {"host": {"pid": 7}}, "starts": [{"at": 1, "runs": {"host": {"pid": 7}}}], "last_activity": 10})
    }

    /// `v` with `key` set to `value`.
    fn with(v: &Value, key: &str, value: Value) -> Value {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert(key.to_owned(), value);
        }
        v
    }

    fn user(seq: u64, client: &str, text: &str) -> Value {
        json!({"id": client, "seq": seq, "at": seq, "kind": "user", "text": text, "client_id": client})
    }

    fn says(seq: u64, text: &str) -> Value {
        json!({"id": format!("a{seq}"), "seq": seq, "at": seq, "kind": "assistant", "text": text})
    }

    /// A refused wait says where the session stands, each of its messages' delivery and what waits in
    /// its queue, and its timeline, each event's kind, message id and first 80 characters (ROOT's
    /// ask, 2026-11-13). The wait it replaces said its status, its exit and its words alone: dropping
    /// any part fails its line.
    #[test]
    fn s35_a_refused_wait_says_where_the_session_stands_and_what_happened() {
        let info = json!({"id": "s1", "status": "idle", "queued": 1, "doing": "Writing", "exit": null, "room_wait": 1234, "last_activity": 99});
        let state = json!({"delivery": {"m0": "completed", "m1": "queued"}, "queue": [{"id": "m1", "text": "do the task again"}]});
        let long = "x".repeat(100);
        let events = vec![
            user(1, "m0", "do the task"),
            says(2, &long),
            json!({"seq": 3, "kind": "tool", "name": "Bash", "status": "running", "output": null, "title": "sleep 600"}),
            json!({"seq": 4, "kind": "notice", "level": "error", "text": "The agent did not keep the requested mode agent; no prompt was sent."}),
        ];
        let text = told(&info, &Ok(state), &events);
        for w in [r#""status":"idle""#, r#""queued":1"#, r#""doing":"Writing""#, r#""room_wait":1234"#, r#""last_activity":99"#] {
            assert!(text.contains(w), "{w} in {text}");
        }
        assert!(!text.contains("exit"), "a null says nothing: {text}");
        assert!(text.contains(r#""m1":"queued""#) && text.contains("m1 do the task again"), "{text}");
        assert!(text.contains("#1 user m0: do the task"), "{text}");
        assert!(text.contains(&format!("#2 assistant: {}…", "x".repeat(80))) && !text.contains(&"x".repeat(81)), "{text}");
        assert!(text.contains("#3 tool Bash running: sleep 600"), "{text}");
        assert!(text.contains("#4 notice error: The agent did not keep"), "{text}");
        // A state that couldn't be read says why; the rest is said still.
        let unread = told(&info, &Err(anyhow::anyhow!("no socket")), &events);
        assert!(unread.contains("couldn't be read: no socket") && unread.contains("#1 user m0"), "{unread}");
        // The refusal itself: what it waited for, why (typed), then all of that.
        let refused = Refused { what: "pi's first turn".into(), stall: Stall::Stuck(Stuck::Answered), evidence: text.clone() };
        assert!(refused.to_string().starts_with("pi's first turn: it is idle") && refused.to_string().ends_with(&text));
    }

    /// Before its harness's first answer a wait has no clock: alive and silent, it waits on however
    /// long (the cold start, the person's ruling of 2026-11-13). A picked bound (the 600 s this
    /// replaces) refuses at the third look.
    #[test]
    fn s35_before_its_first_answer_a_wait_follows_its_harness_alone() {
        let pace = Pace::default();
        assert_eq!(pace.window(Asked::Opening), None);
        let mut w = Watch::new(Asked::Opening, pace.window(Asked::Opening));
        for secs in [0, 1, 600, 86_400] {
            assert_eq!(w.look(&word("running"), &[], None, false, s(secs)), Verdict::Wait, "at {secs} s");
        }
        assert_eq!(w.look(&word("running"), &[], None, true, s(86_401)), Verdict::Done);
    }

    /// Once its harness answered, the slowest answer it gave in the drive bounds how long a wait may
    /// go without progress, and progress moves the window on. Learning the last answer rather than the
    /// slowest, or counting the window from the wait's start rather than its last progress, fails it.
    #[test]
    fn s35_its_slowest_answer_bounds_a_wait_without_progress() {
        let mut pace = Pace::default();
        for took in [3, 11, 2] {
            pace.answered(s(took));
        }
        assert_eq!(pace.slowest(), Some(s(11)));
        let window = pace.window(Asked::Message("m"));
        assert_eq!(window, Some(s(11)));
        let mut w = Watch::new(Asked::Message("m"), window);
        let asked = vec![user(1, "m", "go")];
        assert_eq!(w.look(&word("running"), &asked, None, false, s(0)), Verdict::Wait);
        let line = vec![user(1, "m", "go"), says(2, "line 00000")];
        assert_eq!(w.look(&word("running"), &line, None, false, s(10)), Verdict::Wait);
        assert_eq!(w.look(&word("running"), &line, None, false, s(21)), Verdict::Wait);
        // kiln's word moving is progress too (its queue, here).
        let queued = with(&word("running"), "queued", json!(1));
        assert_eq!(w.look(&queued, &line, None, false, s(30)), Verdict::Wait);
        assert_eq!(w.look(&queued, &line, None, false, s(41)), Verdict::Wait);
        assert_eq!(w.look(&queued, &line, None, false, s(42)), Verdict::Refuse(Stall::Quiet { quiet: s(12), slowest: s(11) }));
    }

    /// A turn the drive asked to be long waits as a first does, whatever it learned: no earlier answer
    /// bounds a turn asked to take longer. Bounding it by the slowest answer refuses it at 2 s.
    #[test]
    fn s35_a_turn_asked_to_be_long_follows_its_harness_alone() {
        let mut pace = Pace::default();
        pace.answered(s(1));
        assert_eq!(pace.window(Asked::Steered("steer")), None);
        let mut w = Watch::new(Asked::Steered("steer"), pace.window(Asked::Steered("steer")));
        for secs in [0, 2, 3_600] {
            assert_eq!(w.look(&word("running"), &[user(1, "steer", "STEER x")], None, false, s(secs)), Verdict::Wait);
        }
    }

    /// Where kiln's word says its harness is gone for good, a wait refuses at once: no runner holds it;
    /// its startup refused while its process lives; its process ended where it never started, or kiln's
    /// next word after its end set no restart. One kiln starts again by itself (`again_at`), or whose
    /// end kiln hasn't decided on yet, is waited on. Refusing every `exited` refuses kiln's restarts.
    #[test]
    fn s35_a_wait_refuses_at_once_where_its_harness_is_gone_for_good() {
        let look = |info: &Value| Watch::new(Asked::Opening, None).look(info, &[], None, false, s(1));
        assert_eq!(look(&word("crashed")), Verdict::Refuse(Stall::Stuck(Stuck::Crashed)));
        let refused = with(&word("exited"), "exit", json!({"error": "Cursor isn't signed in", "at": 5}));
        assert_eq!(look(&refused), Verdict::Refuse(Stall::Stuck(Stuck::Refused { error: Some("Cursor isn't signed in".into()) })));
        let ended = with(&with(&word("exited"), "runs", Value::Null), "exit", json!({"code": 1, "at": 5, "again_at": 7}));
        assert_eq!(look(&ended), Verdict::Wait, "kiln starts it again by itself");
        let never = with(&with(&ended, "exit", json!({"code": 127, "error": "no such file", "at": 5})), "starts", json!([]));
        assert_eq!(look(&never), Verdict::Refuse(Stall::Stuck(Stuck::Ended { code: Some(127), error: Some("no such file".into()) })));
        // Its end, then kiln's word on it: none yet, then one that set no restart.
        let mut w = Watch::new(Asked::Opening, None);
        let gone = with(&ended, "exit", json!({"code": 1, "at": 5}));
        assert_eq!(w.look(&gone, &[], None, false, s(1)), Verdict::Wait);
        assert_eq!(w.look(&gone, &[], None, false, s(2)), Verdict::Wait, "no word of kiln's since");
        let gone = with(&gone, "last_activity", json!(11));
        assert_eq!(w.look(&gone, &[], None, false, s(3)), Verdict::Refuse(Stall::Stuck(Stuck::Ended { code: Some(1), error: None })));
        // An end of another time is a new one, decided on anew.
        let mut w = Watch::new(Asked::Opening, None);
        assert_eq!(w.look(&gone, &[], None, false, s(1)), Verdict::Wait);
        let gone = with(&with(&gone, "exit", json!({"code": 1, "at": 6})), "last_activity", json!(12));
        assert_eq!(w.look(&gone, &[], None, false, s(2)), Verdict::Wait);
        // It waits for a person's answer, which a drive never gives.
        let asks = with(&word("waiting"), "approvals", json!([{"id": "a1", "title": "Run sleep 600"}]));
        assert_eq!(look(&asks), Verdict::Refuse(Stall::Stuck(Stuck::Asks(vec!["Run sleep 600".into()]))));
        // kiln's own work has none of these: it is judged by progress alone.
        assert_eq!(Watch::new(Asked::Kiln, None).look(&word("crashed"), &[], None, false, s(1)), Verdict::Wait);
    }

    /// Idle, with what it was asked delivered and over (its delivery `completed`) and nothing else on
    /// its way, a wait refuses at once: no answer is coming. Whose turn ended only the delivery says:
    /// a message the harness queued itself (Claude Code's, written mid-turn) is still `sent` as it goes
    /// idle. Judging by kiln's word without the delivery refuses that one.
    #[test]
    fn s35_idle_with_its_message_answered_and_nothing_coming_refuses_at_once() {
        let idle = word("idle");
        let events = vec![user(1, "m", "go"), says(2, "STUB_DONE")];
        let over = json!({"delivery": {"m": "completed"}, "queue": []});
        let mut w = Watch::new(Asked::Message("m"), None);
        assert!(w.wants_state(&idle, &events));
        assert_eq!(w.look(&idle, &events, Some(&over), false, s(1)), Verdict::Refuse(Stall::Stuck(Stuck::Answered)));
        let wait = |info: &Value, state: Option<&Value>| Watch::new(Asked::Message("m"), None).look(info, &events, state, false, s(1));
        assert_eq!(wait(&idle, None), Verdict::Wait, "its state unread says nothing");
        assert_eq!(wait(&idle, Some(&json!({"delivery": {"m": "sent"}, "queue": []}))), Verdict::Wait, "the harness holds it");
        assert_eq!(wait(&idle, Some(&json!({"delivery": {"m": "completed", "k": "started"}, "queue": []}))), Verdict::Wait, "another under way");
        assert_eq!(wait(&idle, Some(&json!({"delivery": {"m": "completed"}, "queue": [{"id": "k"}]}))), Verdict::Wait, "kiln's queue holds one");
        for (key, value) in [("queued", json!(1)), ("room_wait", json!(5)), ("pause", json!({"why": "limit"})), ("last_turn", json!({"at": 3, "how": "cut"}))] {
            let held = with(&idle, key, value);
            assert!(!Watch::new(Asked::Message("m"), None).wants_state(&held, &events), "{key}");
        }
        assert!(!w.wants_state(&word("running"), &events));
        // The opening prompt, under kiln's id of the session's own; a message told by words it carries
        // (sent through kiln's MCP tools), under the client id kiln gave it.
        let opening = vec![user(1, &opening_id("s1"), "do the task")];
        let opened = json!({"delivery": {opening_id("s1"): "completed"}, "queue": []});
        assert_eq!(Watch::new(Asked::Opening, None).look(&idle, &opening, Some(&opened), false, s(1)), Verdict::Refuse(Stall::Stuck(Stuck::Answered)));
        let carried = vec![json!({"seq": 1, "at": 1, "kind": "user", "text": "SLOW=2000 keep going long-x1", "client_id": "k77"})];
        assert_eq!(Watch::new(Asked::Message("long-x1"), None).look(&idle, &carried, Some(&json!({"delivery": {"k77": "cancelled"}})), false, s(1)), Verdict::Refuse(Stall::Stuck(Stuck::Answered)));
        assert!(!Watch::new(Asked::Message("long-x2"), None).wants_state(&idle, &carried), "another's words");
    }

    /// A setup wait refuses at once, typed, once a process of the fixture's own ended (its runner, its
    /// server): nothing of it answers again, so it isn't even looked at. Its old deadlines (120 s, 90 s,
    /// 60 s) waited them out; dropping the liveness look waits on, its look answering.
    #[test]
    fn s35_a_setup_wait_refuses_at_once_when_a_partys_process_ended() {
        let gone = SetupStuck::Exited { party: Party::Runner, how: "exit status: 3".into() };
        let mut looks = 0;
        let waited = setup(
            || Some(gone.clone()),
            || {
                looks += 1;
                Ok(Setup::Ready(()))
            },
            |stuck| anyhow::Error::new(SetupRefused { what: "runner box connecting".into(), stuck, evidence: String::new() }),
        );
        assert!(waited.as_ref().err().and_then(|e| e.downcast_ref::<SetupRefused>()).is_some_and(|r| r.stuck == gone), "{waited:?}");
        assert_eq!(looks, 0);
        // Both alive: it looks, and takes its answer.
        assert_eq!(setup(|| None, || Ok(Setup::Ready(7)), |stuck| anyhow::anyhow!("{stuck}")).ok(), Some(7));
    }

    /// The runner connecting (`up`): connected is its answer; listed but not connected, it's between
    /// connections, waited on while its process lives; the server's client error is its final word, its
    /// own trouble (5xx) looked past. Refusing every answer but 200 (the old `get`'s) refuses a 503.
    #[test]
    fn s35_the_runner_connecting_follows_it_not_a_deadline() {
        let agents = json!([{"name": "box", "connected": false}, {"name": "other", "connected": true}]);
        assert!(!connected(&agents, "box"), "between connections");
        assert!(connected(&json!({"agents": [{"name": "box", "connected": true}]}), "box"));
        assert_eq!(server_said::<()>(503, "busy"), Setup::Wait);
        assert_eq!(server_said::<()>(401, "who are you"), Setup::Refuse(SetupStuck::Refused { code: 401, body: "who are you".into() }));
    }

    /// Its headroom off: kiln's word that the runner applied it is the answer; not yet, it's waited on,
    /// as is the server's own trouble with the setting (the old one refused anything but 200 at once).
    #[test]
    fn s35_headroom_off_waits_for_the_runner_to_say_it_applied_it() {
        assert_eq!(headroom_applied(&json!({"enabled": false, "applied": true})), Setup::Ready(()));
        assert_eq!(headroom_applied(&json!({"enabled": false, "applied": false})), Setup::Wait);
        assert_eq!(server_said::<()>(502, "its runner didn't answer"), Setup::Wait);
        assert_eq!(server_said::<()>(404, "no runner box"), Setup::Refuse(SetupStuck::Refused { code: 404, body: "no runner box".into() }));
    }

    /// Its facts on microVMs (`isolation_ready`): they always say `virt.microvm` once they came, so until
    /// then the wait follows the runner; then it's the answer, yes or no and why. The old 60 s took
    /// facts that hadn't come yet for a runner that says nothing of microVMs.
    #[test]
    fn s35_a_runners_word_on_microvms_is_waited_for_not_timed_out() {
        assert_eq!(microvms_said(None), Setup::Wait, "not listed yet");
        assert_eq!(microvms_said(Some(&json!({"name": "box", "facts": null}))), Setup::Wait, "its facts not come yet");
        let says = json!({"name": "box", "facts": {"virt": {"microvm": false}, "isolation": {"why_not": "no hypervisor"}}});
        assert_eq!(microvms_said(Some(&says)), Setup::Ready((false, Some("no hypervisor".into()))));
        assert_eq!(microvms_said(Some(&json!({"facts": {"virt": {"microvm": true}}}))), Setup::Ready((true, None)));
    }

    /// A start (`open`): its session's id; a refusal given while the runner stayed connected is kiln's
    /// word on what was asked, final; the runner between connections, it's asked again. Asking again on
    /// every refusal (the old 90 s) waits out a refusal no retry changes.
    #[test]
    fn s35_a_start_refused_while_its_runner_is_connected_is_final() {
        assert_eq!(opened(200, r#"[{"id": "s1"}]"#, false), Setup::Ready("s1".into()));
        assert_eq!(opened(400, "unknown harness \"x\"", true), Setup::Refuse(SetupStuck::Refused { code: 400, body: "unknown harness \"x\"".into() }));
        assert_eq!(opened(400, "agent:box is not connected", false), Setup::Wait, "between connections");
        assert_eq!(opened(503, "busy", true), Setup::Wait, "the server's own trouble");
        assert_eq!(opened(200, "[]", false), Setup::Refuse(SetupStuck::Refused { code: 200, body: "[]".into() }), "no session");
    }

    /// The stand-in's stopped turn says nothing once it heard, until it's stopped: a line of it after
    /// the stop was taken says it didn't stop, at once. Uncounted (a real harness's), it is judged by
    /// idle and progress alone.
    #[test]
    fn s35_a_stopped_turn_that_says_more_did_not_stop() {
        let heard = vec![user(1, "stop", "HOLD"), says(2, "HEARD HOLD")];
        let mut w = Watch::new(Asked::Stop { said: Some(1) }, None);
        assert_eq!(w.look(&word("running"), &heard, None, false, s(1)), Verdict::Wait);
        let more = vec![user(1, "stop", "HOLD"), says(2, "HEARD HOLD"), says(3, "line 00000")];
        assert_eq!(w.look(&word("running"), &more, None, false, s(2)), Verdict::Refuse(Stall::Stuck(Stuck::NotStopped(vec!["line 00000".into()]))));
        assert_eq!(Watch::new(Asked::Stop { said: None }, None).look(&word("running"), &more, None, false, s(2)), Verdict::Wait);
        assert_eq!(w.look(&word("idle"), &heard, None, true, s(3)), Verdict::Done);
    }
}

// =================================================================================================
// The stand-in, driven as a process: each interface's turn, steer, stop and resume, as kiln's
// adapters speak them.
// =================================================================================================
#[cfg(unix)]
mod stub {
    use serde_json::{Value, json};
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    /// The stand-in as `dialect`, with `args`: its stdin, and its stdout's JSON lines as they come.
    struct Stub {
        child: Child,
        stdin: Option<ChildStdin>,
        out: mpsc::Receiver<Value>,
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The stand-in started in `dir` (none: this test's own folder).
    fn start(dialect: &str, args: &[&str], env: &[(&str, &str)], dir: Option<&std::path::Path>) -> anyhow::Result<Stub> {
        let mut cmd = Command::new("python3");
        cmd.arg(crate::vm_e2e::stub_path()).args(args).env("VM_STUB_PROTOCOL", dialect).envs(env.iter().copied()).stdin(Stdio::piped()).stdout(Stdio::piped());
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let (tx, out) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        Ok(Stub { child, stdin, out })
    }

    impl Stub {
        fn send(&mut self, v: &Value) -> anyhow::Result<()> {
            let s = self.stdin.as_mut().ok_or_else(|| anyhow::anyhow!("stdin closed"))?;
            writeln!(s, "{v}")?;
            Ok(s.flush()?)
        }

        /// Its lines until one `done` takes, that one last.
        fn until(&self, done: impl Fn(&Value) -> bool) -> anyhow::Result<Vec<Value>> {
            let mut got = Vec::new();
            loop {
                let v = self.out.recv_timeout(Duration::from_secs(30)).map_err(|_| anyhow::anyhow!("no line for 30 s; so far {got:?}"))?;
                let end = done(&v);
                got.push(v);
                if end {
                    return Ok(got);
                }
            }
        }
    }

    fn at<'a>(v: &'a Value, p: &str) -> &'a str {
        v.pointer(p).and_then(Value::as_str).unwrap_or("")
    }

    fn said(lines: &[Value], p: &str) -> Vec<String> {
        lines.iter().map(|v| at(v, p).trim().to_owned()).filter(|s| !s.is_empty()).collect()
    }

    #[test]
    fn s34_claude_code_turns_and_its_interrupt() -> anyhow::Result<()> {
        let mut s = start("claude", &["--session-id", "s-1"], &[], None)?;
        s.send(&json!({"type": "control_request", "request_id": "r1", "request": {"subtype": "initialize"}}))?;
        s.until(|v| at(v, "/response/request_id") == "r1")?;
        s.send(&json!({"type": "user", "uuid": "m1", "message": {"role": "user", "content": "do the task"}}))?;
        let turn = s.until(|v| at(v, "/state") == "completed")?;
        let text = said(&turn, "/message/content/0/text");
        assert_eq!(text.first().map(String::as_str), Some(format!("CWD {}", std::env::current_dir()?.display()).as_str()));
        assert!(text.iter().any(|t| t.starts_with("WHERE ")) && text.iter().any(|t| t == "HEARD do the task"));
        assert_eq!(text.last().map(String::as_str), Some("STUB_DONE"));
        assert!(turn.iter().any(|v| at(v, "/type") == "result" && at(v, "/subtype") == "success" && at(v, "/session_id") == "s-1"));
        // A turn stopped mid-way ends as Claude Code's interrupted turn does, its last lines unsaid.
        s.send(&json!({"type": "user", "uuid": "m2", "message": {"role": "user", "content": "SLOW=5000 long"}}))?;
        s.until(|v| at(v, "/message/content/0/text").starts_with("HEARD "))?;
        s.send(&json!({"type": "control_request", "request_id": "r2", "request": {"subtype": "interrupt"}}))?;
        let cut = s.until(|v| at(v, "/state") == "completed")?;
        assert!(cut.iter().any(|v| at(v, "/message/content/0/text") == "[Request interrupted by user]"));
        assert!(cut.iter().any(|v| at(v, "/subtype") == "error_during_execution" && at(v, "/terminal_reason") == "aborted_streaming"));
        assert!(!said(&cut, "/message/content/0/text").iter().any(|t| t == "STUB_DONE"));
        Ok(())
    }

    #[test]
    fn s34_the_stand_in_writes_the_file_its_prompt_names_in_its_folder() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut s = start("claude", &["--session-id", "s-w"], &[], Some(dir.path()))?;
        s.send(&json!({"type": "user", "uuid": "m1", "message": {"role": "user", "content": "WRITE=vm-e2e-x.txt do the task"}}))?;
        let text = said(&s.until(|v| at(v, "/state") == "completed")?, "/message/content/0/text");
        assert!(text.iter().any(|t| t == "WROTE vm-e2e-x.txt"), "{text:?}");
        assert_eq!(crate::vm_e2e::check_written(dir.path(), "vm-e2e-x.txt"), Ok(()));
        Ok(())
    }

    /// A one-request-a-connection HTTP MCP server on loopback, as kiln's `/api/mcp` answers the calls
    /// the stand-in makes (initialize, its notice, tools/list): each request's Authorization header
    /// goes to `seen`.
    fn mcp_server(seen: mpsc::Sender<String>) -> anyhow::Result<u16> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        std::thread::spawn(move || {
            for conn in listener.incoming().map_while(Result::ok) {
                let mut reader = BufReader::new(conn);
                let (mut auth, mut length) = (String::new(), 0usize);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && !line.trim().is_empty() {
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("authorization:") {
                        auth = line.get("authorization:".len()..).unwrap_or("").trim().to_owned();
                    }
                    line.clear();
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let _ = seen.send(auth);
                let msg: Value = serde_json::from_slice(&body).unwrap_or_default();
                let id = msg.get("id").cloned().unwrap_or_default();
                let reply = match msg.get("method").and_then(Value::as_str) {
                    Some("initialize") => json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "kiln"}}}).to_string(),
                    Some("tools/list") => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "runners"}]}}).to_string(),
                    _ => String::new(),
                };
                let status = if reply.is_empty() { "202 Accepted" } else { "200 OK" };
                let mut conn = reader.into_inner();
                let _ = write!(conn, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        Ok(port)
    }

    #[test]
    fn s34_claude_code_and_codex_reach_kilns_mcp_over_http_in_step_18s_forms() -> anyhow::Result<()> {
        let (tx, seen) = mpsc::channel();
        let url = format!("http://127.0.0.1:{}/api/mcp", mcp_server(tx)?);
        // Claude Code: `--mcp-config` JSON, an `http` server with its token in its headers.
        let config = json!({"mcpServers": {"kiln": {"type": "http", "url": url, "headers": {"Authorization": "Bearer claude-token"}}}}).to_string();
        let mut s = start("claude", &["--mcp-config", &config], &[], None)?;
        s.send(&json!({"type": "user", "uuid": "m1", "message": {"role": "user", "content": "MCP_PING do the task"}}))?;
        let text = said(&s.until(|v| at(v, "/state") == "completed")?, "/message/content/0/text");
        assert!(text.iter().any(|t| t == "MCP_OK 1 runners"), "{text:?}");
        // Codex: `-c` settings of a streamable-HTTP server, its token in the variable
        // `bearer_token_env_var` names (dropping that variable's read fails the token check below).
        let mut c = start("codex", &["app-server", "-c", &format!("mcp_servers.kiln.url={}", json!(url)), "-c", "mcp_servers.kiln.bearer_token_env_var=\"KILN_MCP_TOKEN\""], &[("KILN_MCP_TOKEN", "codex-token")], None)?;
        c.send(&json!({"id": 1, "method": "initialize", "params": {}}))?;
        c.send(&json!({"id": 2, "method": "turn/start", "params": {"input": [{"type": "text", "text": "MCP_PING do it"}]}}))?;
        let done = c.until(|v| at(v, "/method") == "turn/completed")?;
        assert!(said(&done, "/params/item/text").iter().any(|t| t == "MCP_OK 1 runners"), "{done:?}");
        let tokens: Vec<String> = seen.try_iter().collect();
        assert!(tokens.iter().any(|t| t == "Bearer claude-token") && tokens.iter().any(|t| t == "Bearer codex-token"), "{tokens:?}");
        Ok(())
    }

    #[test]
    fn s34_codex_app_server_turns_steers_interrupts_and_resumes() -> anyhow::Result<()> {
        let mut s = start("codex", &["app-server"], &[], None)?;
        s.send(&json!({"id": 1, "method": "initialize", "params": {}}))?;
        s.send(&json!({"id": 2, "method": "thread/resume", "params": {"threadId": "th-kept"}}))?;
        let opened = s.until(|v| v.get("id") == Some(&json!(2)))?;
        assert!(opened.iter().any(|v| at(v, "/result/userAgent").starts_with("codex/")));
        assert_eq!(opened.last().map(|v| at(v, "/result/thread/id")), Some("th-kept"), "resumed on its own thread");
        s.send(&json!({"id": 3, "method": "turn/start", "params": {"threadId": "th-kept", "clientUserMessageId": "m1", "input": [{"type": "text", "text": "SLOW=1500 long"}]}}))?;
        let started = s.until(|v| at(v, "/method") == "turn/started")?;
        let turn = started.iter().find_map(|v| v.pointer("/result/turn/id").and_then(Value::as_str)).unwrap_or_default().to_owned();
        assert!(started.iter().any(|v| at(v, "/params/item/clientId") == "m1" && at(v, "/params/threadId") == "th-kept"));
        s.send(&json!({"id": 4, "method": "turn/steer", "params": {"threadId": "th-kept", "expectedTurnId": turn, "clientUserMessageId": "m2", "input": [{"type": "text", "text": "STEER x1"}]}}))?;
        let steered = s.until(|v| at(v, "/params/item/text") == "STEERED STEER x1")?;
        assert!(steered.iter().any(|v| at(v, "/result/turnId") == turn));
        s.send(&json!({"id": 5, "method": "turn/interrupt", "params": {"threadId": "th-kept", "turnId": turn}}))?;
        let end = s.until(|v| at(v, "/method") == "turn/completed")?;
        assert_eq!(end.last().map(|v| at(v, "/params/turn/status")), Some("interrupted"));
        // Its next turn runs to its end; a steer with no turn running is refused, as Codex refuses one.
        s.send(&json!({"id": 6, "method": "turn/start", "params": {"input": [{"type": "text", "text": "do it"}]}}))?;
        let done = s.until(|v| at(v, "/method") == "turn/completed")?;
        assert_eq!(done.last().map(|v| at(v, "/params/turn/status")), Some("completed"));
        assert!(done.iter().any(|v| at(v, "/params/item/text") == "STUB_DONE"));
        s.send(&json!({"id": 7, "method": "turn/steer", "params": {"expectedTurnId": "gone", "input": []}}))?;
        assert!(s.until(|v| v.get("id") == Some(&json!(7)))?.last().is_some_and(|v| v.get("error").is_some()));
        Ok(())
    }

    #[test]
    fn s35_acp_prompts_cancels_and_loads() -> anyhow::Result<()> {
        let mut s = start("acp", &["acp"], &[], None)?;
        s.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}))?;
        let init = s.until(|v| v.get("id") == Some(&json!(1)))?;
        assert_eq!(init.last().and_then(|v| v.pointer("/result/agentCapabilities/loadSession")), Some(&json!(true)));
        s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": "kept", "cwd": "/", "mcpServers": []}}))?;
        s.until(|v| v.get("id") == Some(&json!(2)))?;
        s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "kept", "prompt": [{"type": "text", "text": "MCP_PING do it"}]}}))?;
        let turn = s.until(|v| v.get("id") == Some(&json!(3)))?;
        let text: String = said(&turn, "/params/update/content/text").join("\n");
        assert!(text.contains("MCP_FAIL no kiln MCP server was given") && text.ends_with("STUB_DONE"), "{text}");
        assert!(turn.iter().all(|v| v.get("method").is_none() || at(v, "/params/sessionId") == "kept"));
        assert_eq!(turn.last().map(|v| at(v, "/result/stopReason")), Some("end_turn"));
        s.send(&json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": "kept", "prompt": [{"type": "text", "text": "SLOW=5000 long"}]}}))?;
        s.until(|v| at(v, "/params/update/content/text").starts_with("HEARD "))?;
        s.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "kept"}}))?;
        let cut = s.until(|v| v.get("id") == Some(&json!(4)))?;
        assert_eq!(cut.last().map(|v| at(v, "/result/stopReason")), Some("cancelled"));
        Ok(())
    }

    /// A resolver as a microVM's network answers names (Shards' D236): `granted.test` at 127.0.0.1, and
    /// every other name REFUSED (RFC 1035 4.1.1, RCODE 5), as its network process answers a name no
    /// grant names.
    fn network_answer(q: &[u8]) -> Option<Vec<u8>> {
        let mut at = 12;
        let mut labels = Vec::new();
        loop {
            let n = usize::from(*q.get(at)?);
            if n == 0 {
                break;
            }
            labels.push(String::from_utf8_lossy(q.get(at + 1..at + 1 + n)?).into_owned());
            at += 1 + n;
        }
        let question = q.get(12..at + 5)?;
        let mut out = q.get(..2)?.to_vec();
        if labels.join(".") == "granted.test" {
            out.extend_from_slice(&[0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0]);
            out.extend_from_slice(question);
            out.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1]);
        } else {
            out.extend_from_slice(&[0x81, 0x85, 0, 1, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(question);
        }
        Some(out)
    }

    /// Kiln plan step 17 part 6's probe: the stand-in's `CONNECT=` says what its network answers for a
    /// destination, typed: a granted name's address, connected to; a name its resolver REFUSES, by the
    /// answer's code, as a microVM's network refuses a name no grant names.
    #[test]
    fn s17_the_stand_in_says_what_its_network_answers_for_a_destination() -> anyhow::Result<()> {
        let dns = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let resolver = dns.local_addr()?.to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = dns.recv_from(&mut buf) {
                if let Some(reply) = buf.get(..n).and_then(network_answer) {
                    let _ = dns.send_to(&reply, from);
                }
            }
        });
        let open = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = open.local_addr()?.port();
        let mut s = start("acp", &["acp"], &[("VM_STUB_RESOLVER", resolver.as_str())], None)?;
        s.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}}))?;
        s.until(|v| v.get("id") == Some(&json!(1)))?;
        s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": "kept", "cwd": "/", "mcpServers": []}}))?;
        s.until(|v| v.get("id") == Some(&json!(2)))?;
        let words = format!("CONNECT=granted.test:{port} CONNECT=refused.test:{port} go");
        s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": "kept", "prompt": [{"type": "text", "text": words}]}}))?;
        let turn = s.until(|v| v.get("id") == Some(&json!(3)))?;
        let text = said(&turn, "/params/update/content/text").join("\n");
        assert!(text.contains(&format!("CONNECT_OK granted.test:{port} 127.0.0.1")), "{text}");
        assert!(text.contains(&format!("CONNECT_REFUSED refused.test:{port} dns rcode 5")), "{text}");
        Ok(())
    }

    #[test]
    fn s35_antigravity_takes_kilns_mcp_server_into_its_config_and_turns() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let home_dir = home.path().to_string_lossy().into_owned();
        let added = Command::new("python3").arg(crate::vm_e2e::stub_path()).args(["mcp", "add", "kiln", "/opt/kiln", "reach", "mcp"]).env("VM_STUB_PROTOCOL", "agy").env("HOME", &home_dir).status()?;
        assert!(added.success());
        let config: Value = serde_json::from_slice(&std::fs::read(home.path().join(".gemini/config/mcp_config.json"))?)?;
        assert_eq!(config.pointer("/mcpServers/kiln"), Some(&json!({"command": "/opt/kiln", "args": ["reach", "mcp"]})), "as kiln's agy_has reads it");
        let mut s = start("agy", &["--input-format", "stream-json", "--output-format", "stream-json", "--conversation", "conv-1"], &[("HOME", &home_dir)], None)?;
        let init = s.until(|v| at(v, "/event") == "init")?;
        assert_eq!(init.last().map(|v| at(v, "/conversation_id")), Some("conv-1"));
        s.send(&json!({"event": "user", "message": {"content": "do it"}}))?;
        let turn = s.until(|v| at(v, "/event") == "result")?;
        assert_eq!(said(&turn, "/step_update/text_delta").last().map(String::as_str), Some("STUB_DONE"));
        assert_eq!(turn.last().map(|v| at(v, "/result/status")), Some("SUCCESS"));
        Ok(())
    }

    #[test]
    fn s35_pi_prompts_steers_and_aborts() -> anyhow::Result<()> {
        let mut s = start("pi", &["--mode", "rpc", "--session-id", "pi-kept"], &[], None)?;
        s.send(&json!({"id": "q1", "type": "get_state"}))?;
        let state = s.until(|v| at(v, "/id") == "q1")?;
        assert_eq!(state.last().map(|v| at(v, "/data/sessionId")), Some("pi-kept"));
        s.send(&json!({"id": "m1", "type": "prompt", "message": "SLOW=1500 long"}))?;
        s.until(|v| at(v, "/type") == "agent_start")?;
        s.send(&json!({"id": "m2", "type": "steer", "message": "STEER x2"}))?;
        s.until(|v| at(v, "/assistantMessageEvent/content") == "STEERED STEER x2")?;
        s.send(&json!({"id": "a1", "type": "abort"}))?;
        let end = s.until(|v| at(v, "/type") == "agent_settled")?;
        assert_eq!(end.last().and_then(|v| v.get("aborted")), Some(&json!(true)));
        s.send(&json!({"id": "m3", "type": "prompt", "message": "do it"}))?;
        let done = s.until(|v| at(v, "/type") == "agent_settled")?;
        assert_eq!(done.last().and_then(|v| v.get("aborted")), Some(&json!(false)));
        assert_eq!(said(&done, "/assistantMessageEvent/content").last().map(String::as_str), Some("STUB_DONE"));
        Ok(())
    }

    /// The stand-in's held turn (`HOLD`, the drive's turn to stop) says what it heard, then nothing
    /// more until it's stopped: only a stop ends it, so a line of it after a stop says the stop wasn't
    /// taken. A stand-in that doesn't hold says its lines and STUB_DONE on its own.
    #[test]
    fn s35_a_held_turn_says_nothing_more_until_it_is_stopped() -> anyhow::Result<()> {
        let mut s = start("pi", &["--mode", "rpc"], &[], None)?;
        s.send(&json!({"id": "m1", "type": "prompt", "message": crate::vm_e2e::stub_stopped_turn()}))?;
        s.until(|v| at(v, "/assistantMessageEvent/content").starts_with("HEARD "))?;
        s.send(&json!({"id": "a1", "type": "abort"}))?;
        let rest = s.until(|v| at(v, "/type") == "agent_settled")?;
        assert_eq!(said(&rest, "/assistantMessageEvent/content"), Vec::<String>::new(), "{rest:?}");
        assert_eq!(rest.last().and_then(|v| v.get("aborted")), Some(&json!(true)));
        Ok(())
    }
}

// =================================================================================================
// The drive through kiln, on the host: kiln's own runner, the stand-in behind each harness's name.
// Claude Code and Codex get kiln's MCP server on a Unix host, from the runner's own socket
// (`harness::kinds`, `Kind::launch`: `--mcp-config`, `-c mcp_servers.kiln.*`).
// =================================================================================================
#[cfg(unix)]
mod host {
    /// Past a steer too. kiln frees a message's place in its admission window only when a completed
    /// turn's `result` names it (`harness/claude.rs`, `user_message_uuids`), as the stand-in's does.
    #[test]
    fn s34_claude_code_through_kiln_on_the_host() -> anyhow::Result<()> {
        crate::vm_e2e::on_host("claude", true)
    }

    #[test]
    fn s34_codex_through_kiln_on_the_host() -> anyhow::Result<()> {
        crate::vm_e2e::on_host("codex", true)
    }

    /// A wait on a turn that ended without the word it waits for refuses at once, typed, saying where
    /// the session stands and what it said (kiln plan step 35's waits): the stand-in's first turn over,
    /// idle, its opening prompt's delivery completed and nothing queued. Taking out `Stuck::Answered`
    /// (or naming the opening prompt by another id) leaves it waiting on: a first wait has no clock.
    #[test]
    fn s35_a_wait_on_a_turn_answered_without_its_word_refuses_at_once() -> anyhow::Result<()> {
        use crate::vm_e2e::{Asked, Pace, Refused, Stall, Stuck};
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("SKIP: the stand-in harness needs python3");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let f = crate::vm_e2e::host_fixture(dir.path())?;
        let project = crate::vm_e2e::project(dir.path())?;
        let id = crate::vm_e2e::open(&f, &serde_json::json!({"kind": "pi", "cwd": project, "prompt": "do the task"}))?;
        let mut pace = Pace::default();
        let waited = pace.until(&f, &id, Asked::Opening, "pi's first turn", |_, e| crate::vm_e2e::said_marker(&crate::vm_e2e::said(e), "NEVER-SAID"));
        let refused = waited.as_ref().err().and_then(|e| e.downcast_ref::<Refused>());
        anyhow::ensure!(refused.is_some_and(|r| r.stall == Stall::Stuck(Stuck::Answered)), "{waited:?}");
        anyhow::ensure!(refused.is_some_and(|r| r.evidence.contains(r#""status":"idle""#) && r.evidence.contains("completed") && r.evidence.contains("STUB_DONE")), "{waited:?}");
        pace.stop(&f, &id)
    }
}

// =================================================================================================
// Codex and Claude Code, each in a microVM of its own. Skipped until the runner starts microVMs (kiln
// plan steps 16 and 17); ROOT runs them on each host OS (tests/vm_e2e/README.md). Both get kiln's
// MCP server from inside: the acceptance asks it of both.
// =================================================================================================
mod step34 {
    fn in_its_own_microvm(kind: &str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = crate::vm_e2e::up(dir.path(), "vm", &[])?;
        if !crate::vm_e2e::isolation_ready(&f)? {
            return Ok(());
        }
        let project = crate::vm_e2e::project(dir.path())?;
        crate::vm_e2e::in_microvm(&f, kind, &crate::vm_e2e::builtin_image(kind), &project, true, false)
    }

    #[test]
    fn s34_codex_in_a_microvm_of_its_own() -> anyhow::Result<()> {
        in_its_own_microvm("codex")
    }

    #[test]
    fn s34_claude_code_in_a_microvm_of_its_own() -> anyhow::Result<()> {
        in_its_own_microvm("claude")
    }
}
