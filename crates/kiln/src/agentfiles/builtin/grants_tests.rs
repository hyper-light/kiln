//! What each built-in grants and mounts (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18): its
//! harness's hosts by name (Shards D236, destinations by name), never TCP 443 to any address, and
//! nothing for kiln's MCP, which rides the link kiln drives its harness by (that link's own `mcp` lane,
//! Shards D254), never the network; its project plugged in at its host path (Shards D235), so no
//! volume holds it; and its HOME the person's volume, whole, `/home/<harness>`. Every harness's own
//! tests pin these lines from here. Where each host comes from is docs/research/s18-host-in-vm.

use super::ALL;

/// Each built-in's harness and the hosts it reaches by name, as its NETWORK line names them (sorted, once
/// each), and nothing else ([`destinations`]).
const HOSTS: &[(&str, &[&str])] = &[
    // code.claude.com/docs/en/network-config, "Network access requirements".
    ("claude", &["api.anthropic.com:443", "claude.ai:443", "platform.claude.com:443"]),
    // codex-rs rust-v0.167.2: cli/src/doctor.rs, login/src/server.rs, login/src/auth/manager.rs.
    ("codex", &["api.openai.com:443", "auth.openai.com:443", "chatgpt.com:443"]),
    // gemini-cli v0.68.0 (code_assist/server.ts, oauth2.ts), @google/genai 1.30.0, google-auth-library 10.9.0.
    ("gemini", &["cloudcode-pa.googleapis.com:443", "generativelanguage.googleapis.com:443", "oauth2.googleapis.com:443", "www.googleapis.com:443"]),
    // step 26's probes (its model service), agy 1.5.7's own build (Google's token endpoint).
    ("antigravity", &["cloudcode-pa.googleapis.com:443", "oauth2.googleapis.com:443"]),
    // cursor.com/docs/enterprise/network-configuration, and the CLI 2026.11.11-b912975's own code.
    ("cursor", &["agent.api5.cursor.sh:443", "agent.global.api5.cursor.sh:443", "agent.us.api5.cursor.sh:443", "agentn.api5.cursor.sh:443", "agentn.global.api5.cursor.sh:443", "agentn.us.api5.cursor.sh:443", "api2.cursor.sh:443", "api2direct.cursor.sh:443", "api5.cursor.sh:443", "repo42.cursor.sh:443"]),
    // Devin 3000.12.43's own build (its API servers), step 25's research f14 (its feature flags).
    ("devin", &["api.devin.ai:443", "server.codeium.com:443", "unleash.codeium.com:443"]),
    // Amp 0.0.1794456033-gb2ff22's own build ("AMP_URL optionally overrides ampcode.com").
    ("amp", &["ampcode.com:443"]),
    // Goose 1.59.0: crates/goose/src/providers (declarative and coded), goose-providers' constants.
    ("goose", &["ai-gateway.vercel.sh:443", "ai.salad.cloud:443", "aihubmix.com:443", "api.aigateway.sh:443", "api.aimlapi.com:443", "api.anthropic.com:443", "api.avian.io:443", "api.cerebras.ai:443", "api.deepseek.com:443", "api.empiriolabs.ai:443", "api.eurouter.ai:443", "api.fireworks.ai:443", "api.friendli.ai:443", "api.github.com:443", "api.githubcopilot.com:443", "api.gondola-ai.com:443", "api.groq.com:443", "api.inceptionlabs.ai:443", "api.intelligence.io.solutions:443", "api.kimi.com:443", "api.meta.ai:443", "api.minimax.io:443", "api.mistral.ai:443", "api.novita.ai:443", "api.openai.com:443", "api.opper.ai:443", "api.orcarouter.ai:443", "api.perplexity.ai:443", "api.router.tetrate.ai:443", "api.routerplex.com:443", "api.sakana.ai:443", "api.saygm.com:443", "api.scaleway.ai:443", "api.tensorix.ai:443", "api.together.xyz:443", "api.trustedrouter.com:443", "api.typesafe.ai:443", "api.venice.ai:443", "api.x.ai:443", "api.z.ai:443", "auth.kimi.com:443", "auth.meta.com:443", "auth.openai.com:443", "auth.x.ai:443", "chatgpt.com:443", "cloud-api.near.ai:443", "cloudcode-pa.googleapis.com:443", "dashscope-intl.aliyuncs.com:443", "futurmix.ai:443", "generativelanguage.googleapis.com:443", "github.com:443", "huggingface.co:443", "inference.celeris.ai:443", "integrate.api.nvidia.com:443", "nano-gpt.com:443", "oai.endpoints.kepler.ai.cloud.ovh.net:443", "oauth2.googleapis.com:443", "ollama.com:443", "opencode.ai:443", "openrouter.ai:443", "router.huggingface.co:443", "router.pleum.ai:443", "spark-api-open.xf-yun.com:443", "www.googleapis.com:443"]),
    // OpenCode 2.2.7's pinned build: its provider modules, its sign-ins and its ripgrep; step 22's f21 (models.dev).
    ("opencode", &["ai-gateway.vercel.sh:443", "api.anthropic.com:443", "api.cerebras.ai:443", "api.cohere.ai:443", "api.deepinfra.com:443", "api.deepseek.com:443", "api.fireworks.ai:443", "api.github.com:443", "api.githubcopilot.com:443", "api.groq.com:443", "api.meta.ai:443", "api.minimax.io:443", "api.mistral.ai:443", "api.moonshot.ai:443", "api.openai.com:443", "api.poe.com:443", "api.together.xyz:443", "api.typesafe.ai:443", "api.venice.ai:443", "api.x.ai:443", "api.z.ai:443", "auth.openai.com:443", "chatgpt.com:443", "generativelanguage.googleapis.com:443", "github.com:443", "gitlab.com:443", "inference.baseten.co:443", "inference.do-ai.run:443", "models.dev:443", "opencode.ai:443", "openrouter.ai:443", "release-assets.githubusercontent.com:443"]),
    // pi 1.5.2's pinned build: its model catalog's base URLs, its sign-ins (auth/oauth); step 28's f19 (pi.dev).
    ("pi", &["ai-gateway.vercel.sh:443", "api.ant-ling.com:443", "api.anthropic.com:443", "api.cerebras.ai:443", "api.deepseek.com:443", "api.fireworks.ai:443", "api.github.com:443", "api.groq.com:443", "api.individual.githubcopilot.com:443", "api.kimi.com:443", "api.meta.ai:443", "api.minimax.io:443", "api.minimaxi.com:443", "api.mistral.ai:443", "api.moonshot.ai:443", "api.moonshot.cn:443", "api.openai.com:443", "api.together.ai:443", "api.typesafe.ai:443", "api.x.ai:443", "api.xiaomimimo.com:443", "api.z.ai:443", "auth.kimi.com:443", "auth.meta.com:443", "auth.openai.com:443", "auth.x.ai:443", "chatgpt.com:443", "claude.ai:443", "generativelanguage.googleapis.com:443", "github.com:443", "inference.baseten.co:443", "integrate.api.nvidia.com:443", "open.bigmodel.cn:443", "opencode.ai:443", "openrouter.ai:443", "pi.dev:443", "platform.claude.com:443", "radius.pi.dev:443", "router.huggingface.co:443", "token-plan-ams.xiaomimimo.com:443", "token-plan-cn.xiaomimimo.com:443", "token-plan-sgp.xiaomimimo.com:443", "token-plan.ap-southeast-1.maas.aliyuncs.com:443", "token-plan.cn-beijing.maas.aliyuncs.com:443"]),
];

/// kiln's own port on its machine, where it serves its MCP (its `http_addr`, as `crate::config::Server`
/// defaults it): what an Agentfile granting kiln's MCP over the network would name.
fn kiln_port() -> anyhow::Result<u16> {
    let addr = crate::config::Server::default().http_addr;
    addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).ok_or_else(|| anyhow::anyhow!("kiln's default http_addr {addr} names no port"))
}

/// One built-in's hosts.
fn hosts(harness: &str) -> &'static [&'static str] {
    HOSTS.iter().find(|(h, _)| *h == harness).map(|(_, d)| *d).unwrap_or(&[])
}

/// One built-in's destinations: its hosts, and nothing for kiln's MCP.
fn destinations(harness: &str) -> Vec<String> {
    hosts(harness).iter().map(|h| (*h).to_owned()).collect()
}

/// Its NETWORK, EXPOSE and CONNECT lines, as its Agentfile has them: its destinations by name, its
/// boundary open on their ports alone, and its network its own.
pub(super) fn grant_lines(harness: &str) -> [String; 3] {
    let ds = destinations(harness);
    let egress: Vec<String> = ds.iter().map(|d| format!("--egress={d}")).collect();
    let mut ports: Vec<u16> = ds.iter().filter_map(|d| d.rsplit_once(':')?.1.parse().ok()).collect();
    ports.sort_unstable();
    ports.dedup();
    let ports: Vec<String> = ports.iter().map(|p| format!("{p}/tcp")).collect();
    [
        format!("NETWORK {} provider FOR {harness}", egress.join(" ")),
        format!("EXPOSE --harnesses={harness} {} AS egress FOR provider", ports.join(" ")),
        format!("CONNECT {harness} WITH {harness} ON provider"),
    ]
}

/// Its HOME, the person's volume, whole.
pub(super) fn home(harness: &str) -> String {
    format!("/home/{harness}")
}

/// Its one VOLUME line.
pub(super) fn volume_line(harness: &str) -> String {
    format!("VOLUME --target-kind=harness {} FOR {harness}", home(harness))
}

/// A destination as D236 takes one: a host's name, its labels RFC 1035's and RFC 1123's (letters,
/// digits and inner hyphens, 63 octets at most, 253 in all: RFC 1035 §2.3.4), the last not all digits
/// so it can't read as an address; then a port.
fn destination_ok(d: &str) -> bool {
    let Some((host, port)) = d.rsplit_once(':') else { return false };
    let label = |l: &str| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !l.starts_with('-') && !l.ends_with('-');
    let labels: Vec<&str> = host.split('.').collect();
    port.parse::<u16>().is_ok_and(|p| p > 0) && host.len() <= 253 && labels.len() >= 2 && labels.iter().all(|l| label(l)) && labels.last().is_some_and(|l| !l.bytes().all(|b| b.is_ascii_digit()))
}

/// The Agentfile's instructions: its lines without blanks and comments.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

/// What breaks step 18's grants and mounts in `text`, `harness`'s Agentfile, and its harness files.
fn problems(harness: &str, text: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    let mut found = Vec::new();
    let all = instructions(text);
    let [network, expose, connect] = grant_lines(harness);
    for (keyword, want) in [("NETWORK", network), ("EXPOSE", expose), ("CONNECT", connect)] {
        let had: Vec<&str> = all.iter().copied().filter(|l| l.split(' ').next() == Some(keyword)).collect();
        if had != [want.as_str()] {
            found.push(format!("its {keyword} lines are {had:?}, not `{want}`"));
        }
    }
    let volumes: Vec<&str> = all.iter().copied().filter(|l| l.starts_with("VOLUME")).collect();
    let want = volume_line(harness);
    if volumes != [want.as_str()] {
        found.push(format!("its volumes are {volumes:?}, not `{want}` alone"));
    }
    let want = format!("HOME={}", home(harness));
    for (name, json) in files.iter().filter(|(n, _)| n.starts_with("harness") && n.ends_with(".json")) {
        let h: serde_json::Value = serde_json::from_str(json)?;
        let env: Vec<&str> = h.pointer("/run/env").and_then(serde_json::Value::as_array).map(|a| a.iter().filter_map(serde_json::Value::as_str).collect()).unwrap_or_default();
        let homes: Vec<&str> = env.into_iter().filter(|e| e.starts_with("HOME=")).collect();
        if homes != [want.as_str()] {
            found.push(format!("{name} sets {homes:?}, not `{want}`"));
        }
    }
    Ok(found)
}

/// Every built-in names its harness's hosts and nothing else (sorted, once each), nothing for kiln's MCP
/// among them; mounts no project; and keeps HOME on its one volume. Each guard catches a text the old
/// way or bent.
#[test]
fn step18_every_builtin_names_its_hosts_and_keeps_its_home_on_its_volume() -> anyhow::Result<()> {
    let port = kiln_port()?;
    anyhow::ensure!(HOSTS.len() == ALL.len() && ALL.iter().all(|b| !hosts(b.harness).is_empty()), "a built-in without its hosts here, or a row for none");
    for b in ALL {
        let found = problems(b.harness, b.text, b.files)?;
        anyhow::ensure!(found.is_empty(), "{}: {found:?}", b.name);
        let hosts = hosts(b.harness);
        anyhow::ensure!(hosts.iter().all(|d| d.ends_with(":443") && destination_ok(d)), "{}: {hosts:?}", b.name);
        anyhow::ensure!(hosts.is_sorted() && hosts.windows(2).all(|w| w.first() != w.get(1)), "{}: not sorted once each", b.name);
    }
    let claude = ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no claude"))?;
    let bent = [
        ("TCP 443 to any address", claude.text.replace("NETWORK --egress=api", "NETWORK --dns --egress=443/tcp --egress=api")),
        ("the project at /workspace", format!("{}VOLUME --target-kind=harness /workspace FOR claude\n", claude.text)),
        ("a host more", claude.text.replace("--egress=claude.ai:443", "--egress=claude.ai:443 --egress=example.com:443")),
        ("a host less", claude.text.replace(" --egress=claude.ai:443", "")),
        ("kiln's MCP as a destination on the machine", granting(claude.text, &format!("--egress=host.docker.internal:{port}"), Some(port))),
        ("its boundary open at kiln's port", claude.text.replace(" 443/tcp AS egress", &format!(" 443/tcp {port}/tcp AS egress"))),
        ("its home a folder of the volume", claude.text.replace("/home/claude FOR claude", "/home/claude/.claude FOR claude")),
    ];
    for (what, text) in bent {
        anyhow::ensure!(text != claude.text && !problems("claude", &text, claude.files)?.is_empty(), "{what} passed");
    }
    let scratch: Vec<(&str, String)> = claude.files.iter().map(|(n, t)| (*n, t.replace("HOME=/home/claude", "HOME=/tmp"))).collect();
    let scratch: Vec<(&str, &str)> = scratch.iter().map(|(n, t)| (*n, t.as_str())).collect();
    anyhow::ensure!(!problems("claude", claude.text, &scratch)?.is_empty(), "HOME on the scratch passed");
    for bad in ["example.com", "443", "10.0.0.1:443", "-x.com:443", "a..b:443", "x.com:0", "x.123:443"] {
        anyhow::ensure!(!destination_ok(bad), "{bad} passed");
    }
    Ok(())
}

/// What Shards grants `text`'s microVM, read the way its daemon reads an image's grants (D109): Shards'
/// own parser and `agentfile::grants`, in kiln's process (shards-agentfile, kiln plan step 16).
fn shards_grants(text: &str) -> anyhow::Result<shards_agentfile::agentfile::Grants> {
    use shards_agentfile::{agentfile, instructions, lint::Linter, parser};
    let words = |m: &[u8]| anyhow::anyhow!("{}", String::from_utf8_lossy(m));
    let parsed = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile).map_err(|e| words(&e.message))?;
    let read = instructions::parse(&parsed, &Linter::default()).map_err(|e| words(&e.message))?;
    let directives: Vec<agentfile::Directive> = read
        .stages
        .into_iter()
        .flat_map(|s| s.commands)
        .filter_map(|c| match c.kind {
            instructions::Kind::Agentfile(d) => Some(d),
            _ => None,
        })
        .collect();
    Ok(agentfile::grants(&agentfile::resolve_targets(&directives).map_err(|e| words(&e))?))
}

/// `text` with `grant` added to its network and, given one, `port` opened at its microVM's boundary:
/// an image of kiln's bent to grant more.
fn granting(text: &str, grant: &str, port: Option<u16>) -> String {
    let text = text.replace("NETWORK --egress=api", &format!("NETWORK {grant} --egress=api"));
    match port {
        Some(p) => text.replace(" 443/tcp AS egress", &format!(" 443/tcp {p}/tcp AS egress")),
        None => text,
    }
}

/// Whether Shards' reading of an image grants a way to kiln's MCP over the network: a remote MCP
/// server, or a destination on the machine itself (`host.docker.internal`, Shards D236's name for it).
fn mcp_over_the_network(g: &shards_agentfile::agentfile::Grants) -> bool {
    !g.mcp.is_empty() || g.destinations.iter().any(|d| d.starts_with(b"host.docker.internal:"))
}

/// Shards' own reading of every built-in ([`shards_grants`]): its destinations are its harness's hosts
/// alone. Nothing grants a way to kiln's MCP over the network, which reaches a harness over the link kiln
/// drives it by (Shards D254), and nothing is wider: no port to any address, no other name. An image bent
/// to grant kiln's MCP as a destination on the machine reads as such, and one granting kiln's port or
/// TCP 443 to any address, or any name, reads as wider.
#[test]
fn step18_shards_grants_each_builtin_its_hosts_alone_and_nothing_for_kilns_mcp() -> anyhow::Result<()> {
    for b in ALL {
        let g = shards_grants(b.text)?;
        let mut got: Vec<String> = g.destinations.iter().map(|d| String::from_utf8_lossy(d).into_owned()).collect();
        let mut want = destinations(b.harness);
        got.sort_unstable();
        want.sort_unstable();
        anyhow::ensure!(got == want, "{}: Shards grants {got:?}, not {want:?}", b.name);
        anyhow::ensure!(g.egress.is_empty() && !g.dns && !mcp_over_the_network(&g), "{}: more than its hosts: {g:?}", b.name);
    }
    let claude = ALL.iter().find(|b| b.harness == "claude").ok_or_else(|| anyhow::anyhow!("no claude"))?;
    let port = kiln_port()?;
    let mcp = shards_grants(&granting(claude.text, &format!("--egress=host.docker.internal:{port}"), Some(port)))?;
    anyhow::ensure!(mcp_over_the_network(&mcp), "kiln's MCP as a destination on the machine read as none: {mcp:?}");
    let wider = [
        ("kiln's port to any address", granting(claude.text, &format!("--egress={port}/tcp"), Some(port))),
        ("TCP 443 to any address", granting(claude.text, "--egress=443/tcp", None)),
        ("any name", granting(claude.text, "--dns", None)),
    ];
    for (what, text) in wider {
        let g = shards_grants(&text)?;
        anyhow::ensure!(text != claude.text && (!g.egress.is_empty() || g.dns), "{what} read as no wider: {g:?}");
    }
    Ok(())
}
