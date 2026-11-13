//! Real Codex and Claude Code sessions, each in a Shards microVM of its own, driven by kiln (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 34), over kiln's public surfaces: its REST API, its MCP server
//! and its audit trail. The drive and its checks are `vm_e2e`, which step 35's every-harness runs
//! build on too.
//!
//! - `checks`: the acceptance's checks, against samples (any OS).
//! - `stub`: the stand-in harness (`tests/fixtures/vm-stub/harness/stub`) speaking each harness's interface,
//!   driven as a process (Unix: it's a Python script, as `fake-claude` is).
//! - `host`: Claude Code's and Codex's interfaces driven through kiln on the host, the stand-in behind
//!   each: start, steer, interrupt, stop, resume, a file written in the project, the MCP round trip,
//!   an agent started from where it runs. It shows the drive works against kiln, and that each check
//!   of where a harness ran says "the host" here, as it must (Unix).
//! - `step34`: the same drive with each harness in a microVM of its own. It skips, in the runner's own
//!   words, until the runner says it starts microVMs (kiln plan steps 16 and 17); `VM_E2E=1` makes
//!   that a failure. ROOT runs it on each host OS: `tests/vm_e2e/README.md`.
//!
//! No real harness and no model runs here unless ROOT points these at one (`VM_E2E_*`).
//!
//!   cargo test -p kiln --test vm_harnesses

mod common;
mod vm_e2e;

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
