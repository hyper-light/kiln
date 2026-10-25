//! W24-R29's tests: container operations as W24 units, and the Dockerfile frontend.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};

use super::dockerfile::{self, Became, Build, DEFAULT_PATH, ImageConfig, ImageRef, Options};
use super::*;

fn amd64() -> Platform {
    Platform::new("linux", "amd64", "")
}

fn opts() -> Options {
    Options::new(amd64())
}

/// A Dockerfile from its lines: line `n` is `lines[n - 1]`.
fn df(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

fn lower(src: &str) -> Result<Build> {
    Ok(dockerfile::lower_source(src, &opts())?)
}

fn lower_with(src: &str, o: &Options) -> Result<Build> {
    Ok(dockerfile::lower_source(src, o)?)
}

/// Lowered without kiln's reader's own checks, to reach the frontend's.
fn lower_raw(src: &str) -> std::result::Result<Build, dockerfile::Error> {
    dockerfile::lower(&crate::dockerfile::parse(src).instructions, &opts())
}

fn became(b: &Build, line: u32) -> Result<&Became> {
    Ok(&b.instructions.iter().find(|l| l.line == line).with_context(|| format!("no instruction at line {line}"))?.became)
}

/// The operations line `line` became, its main one last.
fn ops_at(b: &Build, line: u32) -> Result<Vec<NodeId>> {
    match became(b, line)? {
        Became::Ops(ids) => Ok(ids.clone()),
        other => bail!("line {line} became {other:?}"),
    }
}

fn node<'a>(b: &'a Build, id: &NodeId) -> Result<&'a Node> {
    b.graph.get(id).with_context(|| format!("node {id} isn't in the graph"))
}

/// Line `line`'s main operation.
fn main_at(b: &Build, line: u32) -> Result<(NodeId, &Node)> {
    let id = *ops_at(b, line)?.last().with_context(|| format!("line {line} made nothing"))?;
    Ok((id, node(b, &id)?))
}

fn exec_at(b: &Build, line: u32) -> Result<&Exec> {
    match &main_at(b, line)?.1.op {
        Op::Exec(e) => Ok(e),
        op => bail!("line {line} is {}, not an exec", op.kind()),
    }
}

/// Line `line`'s copy (with `--link`, the operation before its merge).
fn copy_at(b: &Build, line: u32) -> Result<(NodeId, &CopyFiles, &Node)> {
    for id in ops_at(b, line)?.iter().rev() {
        let n = node(b, id)?;
        if let Op::Copy(c) = &n.op {
            return Ok((*id, c, n));
        }
    }
    bail!("line {line} made no copy")
}

fn config_at(b: &Build, line: u32) -> Result<&ConfigEdit> {
    match &main_at(b, line)?.1.op {
        Op::Config(c) => Ok(c),
        op => bail!("line {line} is {}, not a config edit", op.kind()),
    }
}

fn input<'a>(n: &'a Node, role: &str) -> Result<&'a Input> {
    n.inputs.iter().find(|i| i.role == role).with_context(|| format!("no input `{role}` in {:?}", n.inputs))
}

fn image(reference: &str) -> Op {
    Op::Image(Image { reference: reference.into(), digest: None, platform: amd64() })
}

fn shell(script: &str) -> Command {
    Command::Shell { shell: vec!["/bin/sh".into(), "-c".into()], script: script.into() }
}

fn exec(command: Command, mounts: Vec<Mount>) -> Op {
    Op::Exec(Exec { command, env: BTreeMap::new(), cwd: "/".into(), user: String::new(), mounts, network: Network::Default, security: Security::Sandbox, platform: amd64(), proxy: BTreeMap::new() })
}

fn local_source(path: &str) -> CopySource {
    CopySource { path: path.into(), follow_symlinks: true, contents_only: true, wildcard: true, unpack: false, include: Vec::new(), required: Vec::new() }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// Each instruction's main operation's planned key (no output digest known), by line.
fn planned(b: &Build) -> Result<BTreeMap<u32, key::Key>> {
    let mut out = BTreeMap::new();
    for l in &b.instructions {
        if let Became::Ops(ids) = &l.became {
            let id = ids.last().with_context(|| format!("line {} made nothing", l.line))?;
            out.insert(l.line, b.graph.key(id, |_| None).context("no key")?);
        }
    }
    Ok(out)
}

#[test]
fn w24_r29_node_ids_are_content_addresses() -> Result<()> {
    let ctx = Op::Local(Local { name: "context".into(), include: Vec::new(), exclude: Vec::new() });
    let mut a = Graph::new();
    let img = a.add(image("docker.io/library/alpine:3.20"), Vec::new())?;
    let ctx_a = a.add(ctx.clone(), Vec::new())?;
    ensure!(a.add(image("docker.io/library/alpine:3.20"), Vec::new())? == img && a.len() == 2, "the same operation again is the same node");
    // Built in the other order, the same ids.
    let mut b = Graph::new();
    let ctx_b = b.add(ctx, Vec::new())?;
    let img_b = b.add(image("docker.io/library/alpine:3.20"), Vec::new())?;
    ensure!((img, ctx_a) == (img_b, ctx_b));

    let copy = |dest: &str| Op::Copy(CopyFiles { dest: dest.into(), sources: vec![local_source("/a")], chown: None, chmod: None, exclude: Vec::new() });
    let inputs = |dest: NodeId, src: NodeId| vec![Input::new("dest", dest), Input::new("src.0", src).reading(["/a".to_owned()])];
    let c = a.add(copy("/app/"), inputs(img, ctx_a))?;
    ensure!(c != a.add(copy("/srv/"), inputs(img, ctx_a))?, "another operation");
    ensure!(c != a.add(copy("/app/"), inputs(ctx_a, img))?, "other inputs");
    ensure!(c != a.add(copy("/app/"), vec![Input::new("dest", img), Input::new("src.0", ctx_a)])?, "reading all of an input, not /a");
    ensure!(c != a.add(copy("/app/"), vec![Input::new("rootfs", img), Input::new("src.0", ctx_a).reading(["/a".to_owned()])])?, "an input in another role");

    // A merge's order is what it does.
    let m1 = a.add(Op::Merge, vec![Input::new("input.0", img), Input::new("input.1", ctx_a)])?;
    ensure!(m1 != a.add(Op::Merge, vec![Input::new("input.0", ctx_a), Input::new("input.1", img)])?);

    // Inputs must be there already (so there are no cycles), each in a role of its own.
    ensure!(matches!(a.add(Op::Merge, vec![Input::new("input.0", NodeId([7; 32]))]), Err(GraphError::Missing { .. })));
    ensure!(matches!(a.add(Op::Merge, vec![Input::new("input.0", img), Input::new("input.0", ctx_a)]), Err(GraphError::Duplicate(_))));

    ensure!(a.ancestors(&c) == BTreeSet::from([img, ctx_a]));
    ensure!(a.dependents(&img).contains(&c) && a.dependents(&img).contains(&m1) && !a.dependents(&c).contains(&img));
    let order = a.topo();
    let at = |id: NodeId| order.iter().position(|x| *x == id).context("not in the order");
    ensure!(order.len() == a.len() && at(img)? < at(c)? && at(ctx_a)? < at(c)? && at(ctx_a)? < at(m1)?);
    ensure!(a.topo() == order, "the order is always the same");
    Ok(())
}

#[test]
fn w24_r29_canonical_text_holds_what_decides_an_operation() -> Result<()> {
    // The same process in shell or exec form, with or without proxies, is the same operation.
    let shell_form = exec(shell("make"), Vec::new());
    let mut exec_form = exec(Command::Argv(strings(&["/bin/sh", "-c", "make"])), Vec::new());
    if let Op::Exec(e) = &mut exec_form {
        e.proxy.insert("HTTP_PROXY".into(), "http://proxy:3128".into());
    }
    ensure!(shell_form.canonical() == exec_form.canonical());
    ensure!(Graph::id_of(&shell_form, &[]) == Graph::id_of(&exec_form, &[]));
    // Everything else that decides it changes it.
    let mut other = shell_form.clone();
    if let Op::Exec(e) = &mut other {
        e.env.insert("A".into(), "1".into());
    }
    ensure!(other.canonical() != shell_form.canonical());
    // Strings are quoted, so values can't pose as other fields or entries.
    let a = Op::Config(ConfigEdit::Label(BTreeMap::from([("a".to_owned(), "b\",\"c\":\"d".to_owned())])));
    let b = Op::Config(ConfigEdit::Label(BTreeMap::from([("a".to_owned(), "b".to_owned()), ("c".to_owned(), "d".to_owned())])));
    ensure!(a.canonical() != b.canonical());
    // A map's order is its keys', however it was filled.
    let mut x = BTreeMap::new();
    x.insert("z".to_owned(), "1".to_owned());
    x.insert("a".to_owned(), "2".to_owned());
    let y: BTreeMap<String, String> = [("a".to_owned(), "2".to_owned()), ("z".to_owned(), "1".to_owned())].into_iter().collect();
    ensure!(Op::Config(ConfigEdit::Env(x)).canonical() == Op::Config(ConfigEdit::Env(y)).canonical());
    Ok(())
}

#[test]
fn w24_r29_a_node_is_a_w24_action_unit() -> Result<()> {
    let mut g = Graph::new();
    let base = g.add(image("docker.io/library/alpine:3.20"), Vec::new())?;
    let mounts = vec![
        Mount { target: "/run/secrets/token".into(), kind: MountKind::Secret { id: "token".into(), env: None, required: true, mode: None, uid: None, gid: None } },
        Mount { target: String::new(), kind: MountKind::Ssh { id: "default".into(), required: false, mode: None, uid: None, gid: None } },
    ];
    let run = g.add(exec(shell("make"), mounts), vec![Input::new("rootfs", base)])?;
    let u = g.unit(&run, |_| None).context("no unit")?;
    ensure!(u.kind == key::Kind::Action);
    ensure!(u.definition == node_op(&g, &run)?.canonical());
    ensure!(u.upstream == BTreeMap::from([("rootfs".to_owned(), format!("node:{}", base.hex()))]), "{:?}", u.upstream);
    ensure!(u.secrets == BTreeSet::from(["token".to_owned(), "ssh:default".to_owned()]), "secrets by name only (W24-R05)");
    ensure!(u.env.is_empty() && u.inputs.is_empty() && u.trust.is_empty() && u.platform == key::Platform::default(), "nothing from outside the operation");
    let planned = g.key(&run, |_| None).context("no key")?;
    ensure!(planned == key::weak(&u));

    // Once its input's output is known, its digest stands in for it (W24-R03).
    let digest = |d: &'static str| move |_: &Input| Some(d.to_owned());
    let known = g.key(&run, digest("sha256:aaa")).context("no key")?;
    ensure!(known != planned);
    // Early cutoff: over another image that gave the same bytes, the same operation keys alike;
    // over other bytes, it doesn't.
    let other_base = g.add(image("docker.io/library/alpine:3.20.1"), Vec::new())?;
    let other_run = g.add(node_op(&g, &run)?.clone(), vec![Input::new("rootfs", other_base)])?;
    ensure!(other_run != run);
    ensure!(g.key(&other_run, digest("sha256:aaa")) == Some(known));
    ensure!(g.key(&other_run, digest("sha256:bbb")) != Some(known));
    ensure!(g.key(&other_run, |_| None) != Some(planned), "planned, they differ: their images do");
    // The digest asked for is of what the input reads.
    let ctx = g.add(Op::Local(Local { name: "context".into(), include: Vec::new(), exclude: Vec::new() }), Vec::new())?;
    let cp = g.add(Op::Copy(CopyFiles { dest: "/".into(), sources: vec![local_source("/Cargo.toml")], chown: None, chmod: None, exclude: Vec::new() }), vec![Input::new("dest", base), Input::new("src.0", ctx).reading(["/Cargo.toml".to_owned()])])?;
    let seen = std::cell::RefCell::new(Vec::new());
    let u = g.unit(&cp, |i| {
        seen.borrow_mut().push((i.role.clone(), i.paths.clone()));
        None
    });
    ensure!(u.is_some());
    ensure!(*seen.borrow() == vec![("dest".to_owned(), Vec::new()), ("src.0".to_owned(), vec!["/Cargo.toml".to_owned()])]);
    ensure!(g.keys(|_| None).len() == g.len());
    Ok(())
}

fn node_op<'a>(g: &'a Graph, id: &NodeId) -> Result<&'a Op> {
    Ok(&g.get(id).context("no node")?.op)
}

#[test]
fn w24_r29_multi_stage_builds() -> Result<()> {
    let src = df(&[
        "ARG BASE=alpine:3.20",                       // 1
        "FROM ${BASE} AS build",                      // 2
        "WORKDIR /src",                               // 3
        "COPY . .",                                   // 4
        "RUN make",                                   // 5
        "FROM build AS test",                         // 6
        "RUN make test",                              // 7
        "FROM scratch AS out",                        // 8
        "COPY --from=build /src/app /app",            // 9
        "FROM alpine:3.20",                           // 10
        "COPY --from=out /app /usr/local/bin/app",    // 11
        "COPY --from=1 /src/report /report",          // 12
        "COPY --from=busybox:1.36 /bin/busybox /bin/", // 13
    ]);
    let b = lower(&src)?;
    ensure!(b.stages.iter().map(|s| s.name.as_str()).collect::<Vec<_>>() == ["build", "test", "out", ""]);
    ensure!(b.target == 3 && b.stages.iter().all(|s| s.reachable));
    ensure!(*became(&b, 1)? == Became::Scope && b.instructions.first().is_some_and(|l| l.stage.is_none()));
    let (img, img_node) = main_at(&b, 2)?;
    ensure!(img_node.op == image("docker.io/library/alpine:3.20"));
    ensure!(main_at(&b, 10)?.0 == img, "the same image in two stages is one node");

    let wd = ops_at(&b, 3)?;
    let mkdir = *wd.first().context("no mkdir")?;
    ensure!(matches!(&node(&b, &mkdir)?.op, Op::Mkdir(m) if m.path == "/src" && m.parents && m.mode == 0o755 && m.owner.is_none()));
    ensure!(input(node(&b, &mkdir)?, "dest")?.node == img);
    let (c4, copy4, n4) = copy_at(&b, 4)?;
    ensure!(copy4.dest == "/src/" && copy4.sources == vec![local_source("/")]);
    ensure!(input(n4, "dest")?.node == mkdir);
    let ctx = input(n4, "src.0")?;
    ensure!(matches!(&node(&b, &ctx.node)?.op, Op::Local(l) if l.name == "context") && ctx.paths == ["/"]);
    let (run5, n5) = main_at(&b, 5)?;
    ensure!(exec_at(&b, 5)?.cwd == "/src" && input(n5, "rootfs")?.node == c4);

    // FROM an earlier stage carries on from it.
    ensure!(*became(&b, 6)? == Became::Scope);
    let (run7, n7) = main_at(&b, 7)?;
    ensure!(input(n7, "rootfs")?.node == run5 && exec_at(&b, 7)?.cwd == "/src");

    // FROM scratch: copies land on nothing.
    ensure!(*became(&b, 8)? == Became::Scope);
    let (c9, copy9, n9) = copy_at(&b, 9)?;
    ensure!(n9.inputs.iter().all(|i| i.role != "dest") && copy9.dest == "/app");
    ensure!(input(n9, "src.0")?.node == run5 && input(n9, "src.0")?.paths == ["/src/app"]);

    let (c11, _, n11) = copy_at(&b, 11)?;
    ensure!(input(n11, "dest")?.node == img && input(n11, "src.0")?.node == c9);
    let (c12, _, n12) = copy_at(&b, 12)?;
    ensure!(input(n12, "dest")?.node == c11 && input(n12, "src.0")?.node == run7, "--from=1 is the stage at index 1");
    let (c13, copy13, n13) = copy_at(&b, 13)?;
    let busybox = input(n13, "src.0")?.node;
    ensure!(node(&b, &busybox)?.op == image("docker.io/library/busybox:1.36") && copy13.dest == "/bin/");

    // The target's image: its config (its image's, with no edits) and its layers.
    let export = node(&b, &b.export)?;
    ensure!(export.op == Op::Export(Export { platform: amd64(), epoch: None }));
    let roles: Vec<(&str, NodeId)> = export.inputs.iter().map(|i| (i.role.as_str(), i.node)).collect();
    ensure!(roles == [("config", img), ("layer.0", img), ("layer.1", c11), ("layer.2", c12), ("layer.3", c13)], "{roles:?}");
    let alpine = ImageRef { reference: "docker.io/library/alpine:3.20".into(), platform: amd64() };
    let busybox_ref = ImageRef { reference: "docker.io/library/busybox:1.36".into(), platform: amd64() };
    ensure!(b.unresolved == BTreeSet::from([alpine, busybox_ref]));

    // --target picks the stage; the others aren't needed.
    let mut o = opts();
    o.target = Some("Build".into());
    let t = lower_with(&src, &o)?;
    ensure!(t.target == 0 && t.stages.iter().map(|s| s.reachable).collect::<Vec<_>>() == [true, false, false, false]);
    let layers: Vec<NodeId> = node(&t, &t.export)?.inputs.iter().filter(|i| i.role.starts_with("layer.")).map(|i| i.node).collect();
    ensure!(layers == [img, mkdir, c4, run5]);
    ensure!(input(node(&t, &t.export)?, "config")?.node == *wd.last().context("no workdir edit")?);
    o.target = Some("nope".into());
    ensure!(dockerfile::lower_source(&src, &o).is_err_and(|e| e.message.contains("could not be found")));
    // A build argument picks the base.
    let mut o = opts();
    o.build_args.insert("BASE".into(), "alpine:3.19".into());
    ensure!(main_at(&lower_with(&src, &o)?, 2)?.1.op == image("docker.io/library/alpine:3.19"));
    Ok(())
}

#[test]
fn w24_r29_args_and_variables_expand_where_dockerfiles_say() -> Result<()> {
    let src = df(&[
        "ARG ALPINE=3.20",                                                 // 1
        "ARG VERSION=1.0",                                                 // 2
        "ARG UNSET",                                                       // 3
        "FROM alpine:${ALPINE} AS app",                                    // 4
        "ARG VERSION",                                                     // 5
        "ARG LOCAL=in-${VERSION}",                                         // 6
        "ARG TARGETARCH",                                                  // 7
        "ARG UNSET",                                                       // 8
        "ENV APP=/opt/app-${VERSION} EMPTY= NAME=\"two words\" B=$APP",    // 9
        "ENV SEE=$APP DFLT=${UNSET:-fallback} ALT=${VERSION:+v$VERSION}",  // 10
        "LABEL version=$VERSION \"quoted key\"=x",                         // 11
        "WORKDIR $APP/bin",                                                // 12
        "RUN echo $VERSION $LOCAL $TARGETARCH",                            // 13
        "ENV legacy form with spaces",                                     // 14
    ]);
    let b = lower(&src)?;
    ensure!(main_at(&b, 4)?.1.op == image("docker.io/library/alpine:3.20"));
    for line in [1, 2, 3, 5, 6, 7, 8] {
        ensure!(*became(&b, line)? == Became::Scope, "line {line}");
    }
    let env9 = BTreeMap::from([("APP".to_owned(), "/opt/app-1.0".to_owned()), ("B".to_owned(), String::new()), ("EMPTY".to_owned(), String::new()), ("NAME".to_owned(), "two words".to_owned())]);
    ensure!(*config_at(&b, 9)? == ConfigEdit::Env(env9), "every pair sees the variables as they were before the instruction");
    ensure!(*config_at(&b, 11)? == ConfigEdit::Label(BTreeMap::from([("quoted key".to_owned(), "x".to_owned()), ("version".to_owned(), "1.0".to_owned())])));
    ensure!(*config_at(&b, 12)? == ConfigEdit::Workdir("/opt/app-1.0/bin".into()));
    ensure!(*config_at(&b, 14)? == ConfigEdit::Env(BTreeMap::from([("legacy".to_owned(), "form with spaces".to_owned())])));
    let run = exec_at(&b, 13)?;
    ensure!(run.command == shell("echo $VERSION $LOCAL $TARGETARCH"), "RUN isn't expanded: its shell does that");
    let want: BTreeMap<String, String> = [
        ("ALT", "v1.0"),
        ("APP", "/opt/app-1.0"),
        ("B", ""),
        ("DFLT", "fallback"),
        ("EMPTY", ""),
        ("LOCAL", "in-1.0"),
        ("NAME", "two words"),
        ("PATH", DEFAULT_PATH),
        ("SEE", "/opt/app-1.0"),
        ("TARGETARCH", "amd64"),
        ("VERSION", "1.0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    ensure!(run.env == want, "{:?}", run.env);
    ensure!(run.cwd == "/opt/app-1.0/bin" && run.proxy.is_empty());

    // Build arguments override defaults; undeclared ones stay out, but for proxies, which the
    // process gets and its key doesn't.
    let mut o = opts();
    o.build_args = [("ALPINE", "3.19"), ("VERSION", "2.0"), ("UNDECLARED", "x"), ("http_proxy", "http://proxy-a:3128")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    let b2 = lower_with(&src, &o)?;
    ensure!(main_at(&b2, 4)?.1.op == image("docker.io/library/alpine:3.19"));
    let run2 = exec_at(&b2, 13)?;
    ensure!(run2.env.get("VERSION").map(String::as_str) == Some("2.0") && run2.env.get("LOCAL").map(String::as_str) == Some("in-2.0"));
    ensure!(!run2.env.contains_key("UNDECLARED") && !run2.env.contains_key("http_proxy"));
    ensure!(run2.proxy == BTreeMap::from([("HTTP_PROXY".to_owned(), "http://proxy-a:3128".to_owned())]));
    ensure!(planned(&b2)?.get(&13) != planned(&b)?.get(&13), "another VERSION, another key");
    o.build_args.insert("http_proxy".into(), "http://proxy-b:3128".into());
    ensure!(planned(&lower_with(&src, &o)?)? == planned(&b2)?, "another proxy, the same keys");
    Ok(())
}

#[test]
fn w24_r29_run_forms_mounts_and_heredocs() -> Result<()> {
    let src = df(&[
        "FROM alpine:3.20 AS base",                                     // 1
        "FROM alpine:3.20",                                             // 2
        "WORKDIR /w",                                                   // 3
        "RUN echo shell form",                                          // 4
        "RUN [\"/bin/echo\", \"exec\", \"form\"]",                     // 5
        "RUN --mount=type=cache,target=/root/.cache,sharing=locked --mount=type=bind,from=base,source=/etc,target=etc --mount=type=secret,id=token,env=TOKEN --mount=type=ssh --mount=type=tmpfs,target=/tmp,size=64m --network=none --security=insecure make", // 6
        "RUN --mount=type=bind,target=/ctx,rw cat /ctx/x",              // 7
        "RUN <<EOF",                                                    // 8
        "apk add curl",                                                 // 9
        "EOF",                                                          // 10
        "RUN <<EOF",                                                    // 11
        "#!/usr/bin/env python3",                                       // 12
        "print('hi')",                                                  // 13
        "EOF",                                                          // 14
        "RUN python3 <<PY",                                             // 15
        "print('x')",                                                   // 16
        "PY",                                                           // 17
        "SHELL [\"/bin/bash\", \"-o\", \"pipefail\", \"-c\"]",         // 18
        "RUN echo with bash",                                           // 19
    ]);
    let b = lower(&src)?;
    let img = main_at(&b, 1)?.0;
    let e4 = exec_at(&b, 4)?;
    ensure!(e4.command == shell("echo shell form") && e4.cwd == "/w" && e4.network == Network::Default && e4.security == Security::Sandbox);
    ensure!(exec_at(&b, 5)?.command == Command::Argv(strings(&["/bin/echo", "exec", "form"])));

    let (_, n6) = main_at(&b, 6)?;
    let e6 = exec_at(&b, 6)?;
    let want = vec![
        Mount { target: "/root/.cache".into(), kind: MountKind::Cache { id: "/root/.cache".into(), sharing: Sharing::Locked, source: "/".into(), readonly: false, mode: None, uid: None, gid: None } },
        Mount { target: "/w/etc".into(), kind: MountKind::Bind { source: "/etc".into(), readonly: true } },
        Mount { target: String::new(), kind: MountKind::Secret { id: "token".into(), env: Some("TOKEN".into()), required: false, mode: None, uid: None, gid: None } },
        Mount { target: String::new(), kind: MountKind::Ssh { id: "default".into(), required: false, mode: None, uid: None, gid: None } },
        Mount { target: "/tmp".into(), kind: MountKind::Tmpfs { size: Some(64 << 20) } },
    ];
    ensure!(e6.mounts == want, "{:?}", e6.mounts);
    ensure!(e6.network == Network::None && e6.security == Security::Insecure);
    ensure!(n6.inputs.iter().map(|i| i.role.as_str()).collect::<Vec<_>>() == ["rootfs", "mount.1"], "only the bind mount reads a tree");
    ensure!(input(n6, "mount.1")?.node == img && input(n6, "mount.1")?.paths == ["/etc"], "from the stage `base`");
    ensure!(b.graph.unit(&main_at(&b, 6)?.0, |_| None).context("no unit")?.secrets == BTreeSet::from(["ssh:default".to_owned(), "token".to_owned()]));

    let (_, n7) = main_at(&b, 7)?;
    ensure!(exec_at(&b, 7)?.mounts == [Mount { target: "/ctx".into(), kind: MountKind::Bind { source: "/".into(), readonly: false } }]);
    ensure!(matches!(&node(&b, &input(n7, "mount.0")?.node)?.op, Op::Local(_)), "a bind mount without `from` is the context");

    ensure!(exec_at(&b, 8)?.command == shell("apk add curl\n"), "a lone heredoc is the shell's script");
    let (_, n11) = main_at(&b, 11)?;
    let e11 = exec_at(&b, 11)?;
    ensure!(e11.command == shell("/dev/pipes/EOF"));
    ensure!(e11.mounts == [Mount { target: "/dev/pipes/".into(), kind: MountKind::Bind { source: "/".into(), readonly: true } }]);
    let script = node(&b, &input(n11, "mount.0")?.node)?;
    ensure!(script.op == Op::Mkfile(Mkfile { path: "/EOF".into(), mode: 0o755, data: "#!/usr/bin/env python3\nprint('hi')\n".into(), owner: None }), "a script with its own interpreter is a file it runs");
    ensure!(exec_at(&b, 15)?.command == shell("python3 <<PY\nprint('x')\nPY"), "other heredocs go back to the shell");
    ensure!(*config_at(&b, 18)? == ConfigEdit::Shell(strings(&["/bin/bash", "-o", "pipefail", "-c"])));
    ensure!(exec_at(&b, 19)?.command == Command::Shell { shell: strings(&["/bin/bash", "-o", "pipefail", "-c"]), script: "echo with bash".into() });
    Ok(())
}

#[test]
fn w24_r29_copy_and_add() -> Result<()> {
    let sum = format!("sha256:{}", "a".repeat(64));
    let add_sum = format!("ADD --checksum={sum} https://example.com/x.bin /x.bin");
    let src = df(&[
        "FROM alpine:3.20 AS build",                                            // 1
        "FROM alpine:3.20",                                                     // 2
        "WORKDIR /app",                                                         // 3
        "ENV WHO=world",                                                        // 4
        "COPY --chown=10:20 --chmod=0644 a.txt b/ ./",                          // 5
        "COPY --link --exclude=*.md docs/ /docs/",                              // 6
        "COPY --link --chmod=755 x /x",                                         // 7
        "COPY --parents src/./pkg/**/*.go /go/",                                // 8
        "COPY --from=build /etc/os-release /",                                  // 9
        "COPY <<EOF /etc/motd",                                                 // 10
        "hello $WHO",                                                           // 11
        "EOF",                                                                  // 12
        "COPY <<\"EOF\" /etc/raw",                                              // 13
        "raw $WHO",                                                             // 14
        "EOF",                                                                  // 15
        "ADD https://example.com/files/tool.tar.gz /opt/",                      // 16
        add_sum.as_str(),                                                       // 17
        "ADD https://github.com/moby/buildkit.git#v0.13.0:docs /buildkit-docs", // 18
        "ADD --keep-git-dir git@github.com:moby/buildkit.git /bk",              // 19
        "ADD rootfs.tar.xz /",                                                  // 20
        "ADD --unpack=false archive.tar /keep/",                                // 21
        "COPY [\"with space.txt\", \"/dst dir/\"]",                             // 22
    ]);
    let b = lower(&src)?;
    let img = main_at(&b, 1)?.0;
    let workdir = *ops_at(&b, 3)?.first().context("no mkdir")?;

    let (c5, copy5, n5) = copy_at(&b, 5)?;
    ensure!(*copy5 == CopyFiles { dest: "/app/".into(), sources: vec![local_source("/a.txt"), local_source("/b")], chown: Some("10:20".into()), chmod: Some("0644".into()), exclude: Vec::new() });
    ensure!(input(n5, "dest")?.node == workdir && input(n5, "src.0")?.paths == ["/a.txt"] && input(n5, "src.1")?.paths == ["/b"]);

    // --link: copied onto nothing, then laid over the stage.
    let (c6, copy6, n6) = copy_at(&b, 6)?;
    ensure!(n6.inputs.iter().all(|i| i.role != "dest") && copy6.exclude == ["*.md"] && copy6.dest == "/docs/");
    let (merge, nm) = main_at(&b, 6)?;
    ensure!(nm.op == Op::Merge && input(nm, "input.0")?.node == c5 && input(nm, "input.1")?.node == c6);
    // With --chmod, BuildKit doesn't link it.
    let (_, copy7, n7) = copy_at(&b, 7)?;
    ensure!(input(n7, "dest")?.node == merge && copy7.chmod.as_deref() == Some("755"));

    let (_, copy8, _) = copy_at(&b, 8)?;
    ensure!(copy8.sources == [CopySource { include: strings(&["pkg/**/*.go"]), ..local_source("/src") }] && copy8.dest == "/go/");
    let (_, copy9, n9) = copy_at(&b, 9)?;
    ensure!(input(n9, "src.0")?.node == img && input(n9, "src.0")?.paths == ["/etc/os-release"] && copy9.dest == "/");

    // Heredocs: files of their own, expanded unless their word is quoted.
    let (_, copy10, n10) = copy_at(&b, 10)?;
    ensure!(node(&b, &input(n10, "src.0")?.node)?.op == Op::Mkfile(Mkfile { path: "/EOF".into(), mode: 0o644, data: "hello world\n".into(), owner: None }));
    ensure!(copy10.dest == "/etc/motd" && copy10.sources.first().map(|s| s.path.as_str()) == Some("/EOF"));
    let (_, _, n13) = copy_at(&b, 13)?;
    ensure!(matches!(&node(&b, &input(n13, "src.0")?.node)?.op, Op::Mkfile(m) if m.data == "raw $WHO\n"));

    // ADD's URLs and git repositories.
    let (_, copy16, n16) = copy_at(&b, 16)?;
    ensure!(node(&b, &input(n16, "src.0")?.node)?.op == Op::Http(Http { url: "https://example.com/files/tool.tar.gz".into(), checksum: None, filename: "tool.tar.gz".into() }));
    ensure!(copy16.sources.first().is_some_and(|s| s.path == "tool.tar.gz" && !s.unpack && !s.wildcard) && copy16.dest == "/opt/");
    let (_, _, n17) = copy_at(&b, 17)?;
    ensure!(matches!(&node(&b, &input(n17, "src.0")?.node)?.op, Op::Http(h) if h.checksum.as_deref() == Some(sum.as_str()) && h.filename == "x.bin"));
    let (_, copy18, n18) = copy_at(&b, 18)?;
    let git = Git { url: "https://github.com/moby/buildkit.git".into(), reference: "v0.13.0".into(), subdir: "docs".into(), checksum: None, keep_git_dir: false, submodules: true };
    ensure!(node(&b, &input(n18, "src.0")?.node)?.op == Op::Git(git) && copy18.sources.first().is_some_and(|s| s.path == "/"));
    let (_, _, n19) = copy_at(&b, 19)?;
    ensure!(matches!(&node(&b, &input(n19, "src.0")?.node)?.op, Op::Git(g) if g.url == "git@github.com:moby/buildkit.git" && g.keep_git_dir && g.reference.is_empty()));
    ensure!(copy_at(&b, 20)?.1.sources == [CopySource { unpack: true, ..local_source("/rootfs.tar.xz") }], "ADD unpacks local archives");
    ensure!(copy_at(&b, 21)?.1.sources == [local_source("/archive.tar")]);
    let (_, copy22, _) = copy_at(&b, 22)?;
    ensure!(copy22.sources == [local_source("/with space.txt")] && copy22.dest == "/dst dir/", "the exec form keeps spaces");

    let bad: &[(&str, &str)] = &[
        ("FROM alpine:3.20\nCOPY https://example.com/a /a\n", "can't be a URL for COPY"),
        ("FROM alpine:3.20\nCOPY git@github.com:a/b.git /b\n", "git ref for COPY"),
        ("FROM alpine:3.20\nCOPY --checksum=sha256:x a /b\n", "unknown flag: checksum"),
        ("FROM alpine:3.20\nADD --checksum=sha256:abc a /b\n", "requires HTTP(S) or Git"),
        ("FROM alpine:3.20\nADD --checksum=md5:abc https://example.com/a /b\n", "unsupported digest algorithm"),
        ("FROM alpine:3.20\nCOPY --link --chown=app a /b\n", "can't be resolved"),
        ("FROM alpine:3.20\nCOPY --chmod=999 a /b\n", "invalid chmod"),
        ("FROM alpine:3.20 AS a\nCOPY --from=b /x /x\nFROM alpine:3.20 AS b\nFROM a\n", "needs to be defined before"),
        ("FROM alpine:3.20\nCOPY --from=7 /x /x\n", "invalid stage index 7"),
        ("FROM alpine:3.20\nARG X=a\nCOPY --from=$X /a /b\n", "variable expansion is not supported"),
        ("FROM alpine:3.20\nADD --keep-git-dir=false https://github.com/a/b.git?keep-git-dir=true /b\n", "inconsistent keep-git-dir"),
        ("FROM alpine:3.20\nCOPY a\n", "at least two arguments"),
        ("FROM alpine:3.20\nCOPY a <<EOF\nx\nEOF\n", "heredoc as a destination"),
    ];
    for (src, needle) in bad {
        match lower_raw(src) {
            Ok(_) => bail!("{src:?} lowered"),
            Err(e) => ensure!(e.message.contains(needle) && e.line >= 2, "{src:?}: {e}"),
        }
    }
    Ok(())
}

#[test]
fn w24_r29_image_config_edits() -> Result<()> {
    let src = df(&[
        "FROM alpine:3.20",                    // 1
        "ENV PORT=9000",                       // 2
        "LABEL a=b c=\"d e\"",                 // 3
        "USER app:app",                        // 4
        "WORKDIR relative",                    // 5
        "EXPOSE 80 443/udp 8000-8002 $PORT",   // 6
        "VOLUME [\"/data\", \"/logs\"]",       // 7
        "VOLUME /cache",                       // 8
        "STOPSIGNAL SIGTERM",                  // 9
        "HEALTHCHECK --interval=30s --timeout=1m30s --start-period=500ms --retries=3 CMD curl -f http://localhost/ || exit 1", // 10
        "MAINTAINER ada",                      // 11
        "CMD serve --port 9000",               // 12
        "ENTRYPOINT [\"/bin/app\"]",           // 13
    ]);
    let b = lower(&src)?;
    let img = main_at(&b, 1)?.0;
    ensure!(*config_at(&b, 2)? == ConfigEdit::Env(BTreeMap::from([("PORT".to_owned(), "9000".to_owned())])));
    ensure!(*config_at(&b, 3)? == ConfigEdit::Label(BTreeMap::from([("a".to_owned(), "b".to_owned()), ("c".to_owned(), "d e".to_owned())])));
    ensure!(*config_at(&b, 4)? == ConfigEdit::User("app:app".into()));
    let wd = ops_at(&b, 5)?;
    ensure!(wd.len() == 2);
    ensure!(matches!(&node(&b, wd.first().context("no mkdir")?)?.op, Op::Mkdir(m) if m.path == "/relative" && m.owner.as_deref() == Some("app:app")), "WORKDIR makes its folder, owned by the user");
    ensure!(*config_at(&b, 5)? == ConfigEdit::Workdir("/relative".into()));
    let ports: BTreeSet<String> = ["80/tcp", "443/udp", "8000/tcp", "8001/tcp", "8002/tcp", "9000/tcp"].into_iter().map(str::to_owned).collect();
    ensure!(*config_at(&b, 6)? == ConfigEdit::Expose(ports));
    ensure!(*config_at(&b, 7)? == ConfigEdit::Volume(BTreeSet::from(["/data".to_owned(), "/logs".to_owned()])));
    ensure!(*config_at(&b, 8)? == ConfigEdit::Volume(BTreeSet::from(["/cache".to_owned()])));
    ensure!(*config_at(&b, 9)? == ConfigEdit::StopSignal("SIGTERM".into()));
    let health = Healthcheck { test: strings(&["CMD-SHELL", "curl -f http://localhost/ || exit 1"]), interval: 30_000_000_000, timeout: 90_000_000_000, start_period: 500_000_000, start_interval: 0, retries: 3 };
    ensure!(*config_at(&b, 10)? == ConfigEdit::Healthcheck(health));
    ensure!(*config_at(&b, 11)? == ConfigEdit::Maintainer("ada".into()));
    ensure!(*config_at(&b, 12)? == ConfigEdit::Cmd(strings(&["/bin/sh", "-c", "serve --port 9000"])));
    ensure!(*config_at(&b, 13)? == ConfigEdit::Entrypoint { args: strings(&["/bin/app"]), keep_cmd: true });

    // The edits chain from the image's config, and the export takes the last.
    let mut chain = Vec::new();
    let mut at = input(node(&b, &b.export)?, "config")?.node;
    while let Some(base) = node(&b, &at)?.inputs.iter().find(|i| i.role == "base") {
        chain.push(at);
        at = base.node;
    }
    ensure!(at == img);
    let lines: Vec<u32> = (2..=13).rev().collect();
    let mains = lines.iter().map(|l| main_at(&b, *l).map(|(id, _)| id)).collect::<Result<Vec<_>>>()?;
    ensure!(chain == mains);

    let other = lower(&df(&["FROM alpine:3.20", "CMD [\"a\"]", "FROM alpine:3.20", "ENTRYPOINT [\"x\"]", "HEALTHCHECK NONE"]))?;
    ensure!(*config_at(&other, 4)? == ConfigEdit::Entrypoint { args: strings(&["x"]), keep_cmd: false }, "an ENTRYPOINT clears a CMD from before its stage");
    ensure!(matches!(config_at(&other, 5)?, ConfigEdit::Healthcheck(h) if h.test == ["NONE"]));

    let bad: &[(&str, &str)] = &[
        ("FROM alpine:3.20\nSTOPSIGNAL SIGNOPE\n", "invalid signal"),
        ("FROM alpine:3.20\nEXPOSE 70000\n", "invalid containerPort"),
        ("FROM alpine:3.20\nEXPOSE 80/xyz\n", "invalid proto"),
        ("FROM alpine:3.20\nHEALTHCHECK --interval=1us CMD x\n", "cannot be less than 1ms"),
        ("FROM alpine:3.20\nHEALTHCHECK --retries=-1 CMD x\n", "cannot be negative"),
        ("FROM alpine:3.20\nHEALTHCHECK --interval=5 CMD x\n", "missing unit"),
        ("FROM alpine:3.20\nHEALTHCHECK GET /\n", "Unknown type"),
        ("FROM alpine:3.20\nSHELL sh -c\n", "JSON form"),
        ("FROM alpine:3.20\nVOLUME [\"\"]\n", "can not be an empty string"),
    ];
    for (src, needle) in bad {
        match lower_raw(src) {
            Ok(_) => bail!("{src:?} lowered"),
            Err(e) => ensure!(e.message.contains(needle) && e.line == 2, "{src:?}: {e}"),
        }
    }
    Ok(())
}

#[test]
fn w24_r29_what_isnt_native_is_named() -> Result<()> {
    let src = df(&[
        "# syntax=example.com/frontends/custom:1",                        // 1
        "FROM alpine:3.20 AS base",                                       // 2
        "ONBUILD RUN echo triggered",                                     // 3
        "FROM base",                                                      // 4
        "RUN --device=nvidia.com/gpu=all nvidia-smi",                     // 5
        "ADD https://github.com/moby/buildkit.git?mtime=commit /bk",      // 6
    ]);
    let b = lower(&src)?;
    let named: Vec<(u32, &str)> = b.not_native.iter().map(|n| (n.line, n.instruction.as_str())).collect();
    ensure!(named == [(1, "syntax"), (3, "ONBUILD"), (4, "FROM"), (5, "RUN"), (6, "ADD")], "{:?}", b.not_native);
    ensure!(matches!(became(&b, 3)?, Became::NotNative(w) if w.contains("ONBUILD")));
    ensure!(b.not_native.iter().all(|n| !n.what.is_empty()));
    // What can still be modelled is, so the graph stays whole.
    ensure!(exec_at(&b, 5)?.command == shell("nvidia-smi"));
    ensure!(matches!(copy_at(&b, 6)?.2.inputs.iter().find(|i| i.role == "src.0").map(|i| node(&b, &i.node)), Some(Ok(Node { op: Op::Git(_), .. }))));

    // Docker's own frontend is kiln's; another one, however it's named, isn't.
    for syntax in ["docker/dockerfile:1", "docker/dockerfile:1.7-labs", "docker.io/docker/dockerfile-upstream:master"] {
        ensure!(lower(&format!("# syntax={syntax}\nFROM alpine:3.20\n"))?.not_native.is_empty(), "{syntax}");
    }
    // BuildKit's switches among the build arguments: BUILDKIT_SYNTAX overrides the directive.
    let mut o = opts();
    o.build_args.insert("BUILDKIT_SYNTAX".into(), "example.com/other:1".into());
    o.build_args.insert("BUILDKIT_CACHE_MOUNT_NS".into(), "team".into());
    o.build_args.insert("BUILDKIT_DOCKERFILE_CHECK".into(), "skip=all".into());
    let s = lower_with("# syntax=docker/dockerfile:1\nFROM alpine:3.20\n", &o)?;
    let named: Vec<(u32, &str, bool)> = s.not_native.iter().map(|n| (n.line, n.instruction.as_str(), n.what.starts_with("BUILDKIT_SYNTAX"))).collect();
    ensure!(named == [(0, "build-arg", false), (0, "build-arg", true)], "{:?}", s.not_native);
    o.build_args.insert("BUILDKIT_SYNTAX".into(), "docker/dockerfile:1.7".into());
    ensure!(lower_with("# syntax=example.com/other:1\nFROM alpine:3.20\n", &o)?.not_native.len() == 1, "only the cache namespace: the build argument's frontend is Docker's");

    // Windows stages, and triggers a base image carries.
    let w = lower("FROM --platform=windows/amd64 mcr.microsoft.com/windows/servercore:ltsc2022\nRUN dir\n")?;
    ensure!(w.not_native.iter().map(|n| (n.line, n.instruction.as_str())).collect::<Vec<_>>() == [(1, "FROM")]);
    ensure!(matches!(&exec_at(&w, 2)?.command, Command::Shell { shell, .. } if *shell == strings(&["cmd", "/S", "/C"])));
    let mut o = opts();
    o.images.insert(ImageRef { reference: "docker.io/library/alpine:3.20".into(), platform: amd64() }, ImageConfig { on_build: strings(&["RUN echo hook"]), ..ImageConfig::default() });
    let t = lower_with("FROM alpine:3.20\nRUN make\n", &o)?;
    ensure!(t.not_native.iter().map(|n| (n.line, n.instruction.as_str())).collect::<Vec<_>>() == [(1, "FROM")]);
    Ok(())
}

#[test]
fn w24_r29_base_images_resolve_in_two_passes() -> Result<()> {
    let src = df(&["FROM alpine:3.20", "ENV P=$PATH:/x", "WORKDIR rel", "RUN make"]);
    let first = lower(&src)?;
    let alpine = ImageRef { reference: "docker.io/library/alpine:3.20".into(), platform: amd64() };
    ensure!(first.unresolved == BTreeSet::from([alpine.clone()]), "it says what to resolve");
    let e = exec_at(&first, 4)?;
    ensure!(e.env.get("P") == Some(&format!("{DEFAULT_PATH}:/x")) && e.cwd == "/rel" && e.user.is_empty(), "unresolved, its config reads as empty");

    let digest = format!("sha256:{}", "b".repeat(64));
    let mut o = opts();
    o.images.insert(alpine, ImageConfig { digest: Some(digest.clone()), env: strings(&["PATH=/custom/bin", "HOME=/root"]), working_dir: "/home".into(), user: "nobody".into(), shell: Vec::new(), on_build: Vec::new(), os: "linux".into() });
    let second = lower_with(&src, &o)?;
    ensure!(second.unresolved.is_empty());
    ensure!(matches!(&main_at(&second, 1)?.1.op, Op::Image(i) if i.digest.as_deref() == Some(digest.as_str())), "the digest pins it");
    let e = exec_at(&second, 4)?;
    ensure!(e.env.get("P").map(String::as_str) == Some("/custom/bin:/x") && e.env.get("HOME").map(String::as_str) == Some("/root"));
    ensure!(e.cwd == "/home/rel" && e.user == "nobody");
    ensure!(matches!(&node(&second, ops_at(&second, 3)?.first().context("no mkdir")?)?.op, Op::Mkdir(m) if m.owner.as_deref() == Some("nobody")));
    ensure!(planned(&first)?.get(&4) != planned(&second)?.get(&4));
    Ok(())
}

#[test]
fn w24_r29_platforms_and_image_names() -> Result<()> {
    ensure!(Platform::parse("linux/aarch64") == Some(Platform::new("linux", "arm64", "")));
    ensure!(Platform::parse("linux/arm64/v8").map(|p| p.to_string()).as_deref() == Some("linux/arm64"));
    ensure!(Platform::parse("linux/arm").map(|p| p.to_string()).as_deref() == Some("linux/arm/v7"));
    ensure!(Platform::parse("Linux/x86_64").map(|p| p.to_string()).as_deref() == Some("linux/amd64"));
    ensure!(Platform::parse("linux").is_none() && Platform::parse("/amd64").is_none() && Platform::parse("a/b/c/d").is_none(), "no guessing the machine");

    // Built on arm64 for amd64.
    let mut o = opts();
    o.build_platform = Platform::new("linux", "arm64", "");
    let src = df(&["FROM --platform=$BUILDPLATFORM golang:1.23 AS build", "ARG TARGETOS TARGETARCH", "RUN GOOS=$TARGETOS GOARCH=$TARGETARCH go build -o /out", "FROM alpine:3.20", "COPY --from=build /out /out"]);
    let b = lower_with(&src, &o)?;
    ensure!(matches!(&main_at(&b, 1)?.1.op, Op::Image(i) if i.platform.arch == "arm64" && i.reference == "docker.io/library/golang:1.23"));
    let e = exec_at(&b, 3)?;
    ensure!(e.platform.arch == "arm64" && e.env.get("TARGETARCH").map(String::as_str) == Some("amd64") && e.env.get("TARGETOS").map(String::as_str) == Some("linux"));
    ensure!(matches!(&main_at(&b, 4)?.1.op, Op::Image(i) if i.platform == amd64()));
    // `scratch` is for the target platform whatever `--platform` says, as in BuildKit.
    ensure!(exec_at(&lower_with("FROM --platform=linux/arm64 scratch\nRUN x\n", &o)?, 2)?.platform == amd64());

    let hex = "c".repeat(64);
    let pinned = format!("alpine@sha256:{hex}");
    let both = format!("alpine:3.20@sha256:{hex}");
    let names: &[(&str, &str, bool)] = &[
        ("alpine", "docker.io/library/alpine:latest", false),
        ("library/alpine:3", "docker.io/library/alpine:3", false),
        ("docker.io/alpine:3", "docker.io/library/alpine:3", false),
        ("index.docker.io/library/alpine", "docker.io/library/alpine:latest", false),
        ("ghcr.io/org/team/img:1.2", "ghcr.io/org/team/img:1.2", false),
        ("localhost:5000/x", "localhost:5000/x:latest", false),
        (pinned.as_str(), "docker.io/library/alpine", true),
        (both.as_str(), "docker.io/library/alpine:3.20", true),
    ];
    for (from, reference, has_digest) in names {
        let b = lower(&format!("FROM {from}\n"))?;
        ensure!(matches!(&main_at(&b, 1)?.1.op, Op::Image(i) if i.reference == *reference && i.digest.is_some() == *has_digest), "{from}: {:?}", main_at(&b, 1)?.1.op);
    }
    for bad in ["Alpine", "alpine:", "a/b//c", "alpine@sha256:xyz", "alpine:bad tag"] {
        ensure!(lower_raw(&format!("FROM {bad}\n")).is_err(), "{bad} lowered");
    }
    Ok(())
}

#[test]
fn w24_r29_errors_are_values_with_their_lines() -> Result<()> {
    let cases: &[(&str, u32, &str)] = &[
        ("RUN echo\nFROM alpine:3.20\n", 1, "before any FROM"),
        ("FROM alpine:3.20 AS 1st\n", 1, "invalid name for build stage"),
        ("FROM alpine:3.20 as\n", 1, "one or three arguments"),
        ("FROM --platform=linux alpine:3.20\n", 1, "os/arch"),
        ("ARG X=\nFROM $X\n", 2, "should not be blank"),
        ("FROM alpine:3.20\nRUN --mount=type=cache,size=1g make\n", 2, "unexpected key 'size'"),
        ("FROM alpine:3.20\nRUN --mount=type=nope make\n", 2, "unsupported mount type"),
        ("FROM alpine:3.20\nRUN --mount=type=secret make\n", 2, "one of source, target required"),
        ("FROM alpine:3.20\nRUN --mount=type=bind,from=$X,target=/x make\n", 2, "doesn't support variable expansion"),
        ("FROM alpine:3.20\nRUN --mount=type=bind,mode=0644,target=/x make\n", 2, "mode not allowed"),
        ("FROM alpine:3.20\nRUN --mount=type=cache,target=/ make\n", 2, "invalid mount target"),
        ("FROM alpine:3.20\nRUN --network=bridge make\n", 2, "invalid network mode"),
        ("FROM alpine:3.20\nRUN --frobnicate make\n", 2, "unknown flag: frobnicate"),
        ("FROM alpine:3.20\nRUN --network make\n", 2, "missing a value"),
        ("FROM alpine:3.20\nENV --x=1 A=b\n", 2, "unknown flag"),
        ("FROM alpine:3.20\nENV A\n", 2, "must have two arguments"),
        ("FROM alpine:3.20\nENV A=1 B\n", 2, "can't find ="),
        ("FROM alpine:3.20\nARG A=${B\n", 2, "missing '}'"),
        ("FROM alpine:3.20\nUSER ${A:?no user}\n", 2, "no user"),
        ("FROM alpine:3.20\nONBUILD ONBUILD RUN x\n", 2, "Chaining ONBUILD"),
        ("FROM alpine:3.20\nCMD [1, 2]\n", 2, "strings only"),
        ("FROM alpine:3.20\nFROBNICATE x\n", 2, "unknown instruction"),
    ];
    for (src, line, needle) in cases {
        match lower_raw(src) {
            Ok(_) => bail!("{src:?} lowered"),
            Err(e) => ensure!(e.line == *line && e.message.contains(needle), "{src:?}: {e}"),
        }
    }
    // Through kiln's reader, its own checks come first.
    ensure!(lower("FROM alpine:3.20\nRUNN x\n").is_err_and(|e| e.to_string().contains("KDF1001")));
    ensure!(lower("# just a comment\n").is_err());
    Ok(())
}

#[test]
fn w24_r29_a_stage_the_target_doesnt_need_may_fail() -> Result<()> {
    let src = df(&[
        "FROM alpine:3.20 AS release",                                  // 1
        "ARG VERSION",                                                  // 2
        "ADD https://example.com/app-${VERSION:?needed}.tgz /",        // 3
        "RUN install",                                                  // 4
        "FROM alpine:3.20 AS dev",                                      // 5
        "RUN echo dev",                                                 // 6
    ]);
    let b = lower(&src)?;
    let release = b.stages.first().context("no stage")?;
    ensure!(!release.reachable && release.error.as_deref().is_some_and(|e| e.contains("VERSION: needed")), "{release:?}");
    ensure!(*became(&b, 2)? == Became::Scope && matches!(became(&b, 3)?, Became::Failed(_)) && matches!(became(&b, 4)?, Became::Failed(_)));
    ensure!(exec_at(&b, 6)?.command == shell("echo dev"));
    let mut o = opts();
    o.target = Some("release".into());
    ensure!(dockerfile::lower_source(&src, &o).is_err_and(|e| e.line == 3), "but the target's own stages must lower");
    o.build_args.insert("VERSION".into(), "1.2".into());
    ensure!(matches!(&node(&lower_with(&src, &o)?, &input(copy_at(&lower_with(&src, &o)?, 3)?.2, "src.0")?.node)?.op, Op::Http(h) if h.url == "https://example.com/app-1.2.tgz"));
    Ok(())
}

#[test]
fn w24_r29_keys_are_stable_and_change_only_downstream() -> Result<()> {
    let lines = [
        "FROM alpine:3.20 AS deps",        // 1
        "RUN apk add build-base",          // 2
        "COPY go.mod go.sum ./",           // 3
        "RUN go mod download",             // 4
        "FROM alpine:3.20 AS docs",        // 5
        "COPY docs/ /docs/",               // 6
        "RUN make-docs /docs",             // 7
        "FROM deps AS build",              // 8
        "COPY . .",                        // 9
        "RUN go build -o /out/app",        // 10
        "FROM alpine:3.20",                // 11
        "COPY --from=build /out/app /app", // 12
        "COPY --from=docs /docs /docs",    // 13
        "CMD [\"/app\"]",                  // 14
    ];
    let src = df(&lines);
    let a = lower(&src)?;
    let again = lower(&src)?;
    ensure!(a == again, "the same Dockerfile lowers alike");
    let keys = planned(&a)?;
    ensure!(keys == planned(&again)?);
    ensure!(a.graph.keys(|_| None) == again.graph.keys(|_| None));

    let edited = |line: usize, text: &str| -> Result<Build> {
        let mut l = lines;
        if let Some(slot) = l.get_mut(line - 1) {
            *slot = text;
        }
        lower(&df(&l))
    };
    let changed = |b: &Build| -> Result<BTreeSet<u32>> {
        let k = planned(b)?;
        Ok(k.iter().filter(|(line, key)| keys.get(*line) != Some(*key)).map(|(line, _)| *line).collect())
    };

    // A step of `deps` changes it, what follows it in its stage, the stages built on it and the
    // copies after them; not the steps before it, not `docs`, and not the final config.
    let e = edited(4, "RUN go mod download -x")?;
    ensure!(changed(&e)? == BTreeSet::from([4, 9, 10, 12, 13]), "{:?}", changed(&e)?);
    ensure!(e.export != a.export);
    let old_run = main_at(&a, 4)?.0;
    let downstream: BTreeSet<NodeId> = [9, 10, 12, 13].iter().map(|l| main_at(&a, *l).map(|(id, _)| id)).collect::<Result<_>>()?;
    ensure!(a.graph.dependents(&old_run).is_superset(&downstream), "the graph agrees on what's downstream");
    // A step of `docs` touches only its own and the copy of its output.
    ensure!(changed(&edited(7, "RUN make-docs --strict /docs")?)? == BTreeSet::from([7, 13]));
    // The final command is config: no layer moves.
    let c = edited(14, "CMD [\"/app\", \"--serve\"]")?;
    ensure!(changed(&c)? == BTreeSet::from([14]) && c.export != a.export);
    Ok(())
}

#[test]
fn w24_r29_ids_and_keys_ignore_hashmap_order() -> Result<()> {
    // Build arguments through HashMaps, each with its own random order.
    let mut lines = vec!["FROM alpine:3.20".to_owned()];
    lines.extend((0..48).map(|i| format!("ARG A{i}")));
    lines.push("RUN env".to_owned());
    lines.push("LABEL z=1 a=2 m=3".to_owned());
    let src = lines.join("\n") + "\n";
    let mut seen = None;
    for _ in 0..8 {
        let args: HashMap<String, String> = (0..48).map(|i| (format!("A{i}"), format!("v{i}"))).collect();
        let mut o = opts();
        o.build_args = args.into_iter().collect();
        let b = lower_with(&src, &o)?;
        let ids: Vec<NodeId> = b.graph.topo();
        let keys = b.graph.keys(|_| None);
        match &seen {
            None => seen = Some((ids, keys)),
            Some((i, k)) => ensure!(*i == ids && *k == keys, "a HashMap's order changed an id or a key"),
        }
    }
    // Pairs in another order are the same edit.
    let a = lower("FROM alpine:3.20\nENV A=1 B=2\nLABEL x=1 y=2\n")?;
    let b = lower("FROM alpine:3.20\nENV B=2 A=1\nLABEL y=2 x=1\n")?;
    ensure!(main_at(&a, 2)?.0 == main_at(&b, 2)?.0 && main_at(&a, 3)?.0 == main_at(&b, 3)?.0);
    // Independent stages, in either order, are the same nodes.
    let one = lower(&df(&["FROM alpine:3.20 AS a", "RUN make a", "FROM busybox:1.36 AS b", "RUN make b", "FROM scratch", "COPY --from=a /a /a", "COPY --from=b /b /b"]))?;
    let two = lower(&df(&["FROM busybox:1.36 AS b", "RUN make b", "FROM alpine:3.20 AS a", "RUN make a", "FROM scratch", "COPY --from=a /a /a", "COPY --from=b /b /b"]))?;
    ensure!(main_at(&one, 2)?.0 == main_at(&two, 4)?.0 && main_at(&one, 4)?.0 == main_at(&two, 2)?.0);
    ensure!(one.export == two.export, "the same image, however the stages were ordered");
    Ok(())
}

/// Every file in the tree named as Dockerfiles are, past build output, dependencies and hidden
/// folders.
fn dockerfiles(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    todo.push(entry.path());
                }
            } else if ty.is_file() && (name == "Dockerfile" || name.starts_with("Dockerfile.") || name.ends_with(".Dockerfile") || name.ends_with(".dockerfile") || name == "Containerfile" || name.starts_with("Containerfile.")) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[test]
fn w24_r29_every_dockerfile_in_the_repository_lowers() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let files = dockerfiles(&root)?;
    ensure!(files.iter().any(|f| f.ends_with("Dockerfile") && f.parent().is_some_and(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())), "kiln's own Dockerfile: {files:?}");
    let mut total = 0;
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut not_native = Vec::new();
    let mut unresolved = BTreeSet::new();
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        let src = std::fs::read_to_string(f).with_context(|| rel.clone())?;
        let parsed = crate::dockerfile::parse(&src);
        let b = dockerfile::lower_source(&src, &opts()).with_context(|| rel.clone())?;
        ensure!(b.instructions.len() == parsed.instructions.len(), "{rel}: every instruction once");
        for ins in &parsed.instructions {
            total += 1;
            *kinds.entry(ins.keyword.clone()).or_default() += 1;
            let l = b.instructions.iter().find(|l| l.line == ins.line).with_context(|| format!("{rel}:{}: {} wasn't lowered", ins.line, ins.keyword))?;
            match &l.became {
                Became::Ops(ids) => {
                    ensure!(!ids.is_empty(), "{rel}:{}: no operations", ins.line);
                    for id in ids {
                        ensure!(b.graph.get(id).is_some_and(|n| n.lines.contains(&ins.line)), "{rel}:{}: its node doesn't know its line", ins.line);
                    }
                }
                Became::Scope => ensure!(matches!(ins.keyword.as_str(), "ARG" | "FROM"), "{rel}:{}: {} made nothing", ins.line, ins.keyword),
                Became::NotNative(_) => ensure!(b.not_native.iter().any(|n| n.line == ins.line), "{rel}:{}: not native, but not named", ins.line),
                Became::Failed(why) => bail!("{rel}:{}: {why}", ins.line),
            }
        }
        not_native.extend(b.not_native.iter().map(|n| format!("{rel}:{} {}: {}", n.line, n.instruction, n.what)));
        unresolved.extend(b.unresolved.iter().map(|r| format!("{} ({})", r.reference, r.platform)));
        println!("{rel}: {} instructions, {} stages, {} operations", parsed.instructions.len(), b.stages.len(), b.graph.len());
    }
    println!("{} Dockerfiles, {total} instructions: {kinds:?}", files.len());
    println!("not native yet: {}", if not_native.is_empty() { "nothing".to_owned() } else { not_native.join("; ") });
    println!("base images to resolve: {}", unresolved.into_iter().collect::<Vec<_>>().join(", "));
    Ok(())
}
