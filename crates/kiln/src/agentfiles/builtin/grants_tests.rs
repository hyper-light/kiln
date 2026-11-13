//! What each built-in grants and mounts (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18): its
//! harness's hosts by name (Shards D236, destinations by name), never TCP 443 to any address, and
//! nothing for kiln's MCP, which rides the link kiln drives its harness by (that link's own `mcp` lane,
//! Shards D254), never the network; its project plugged in at its host path (Shards D235), so no
//! volume holds it; and its HOME the person's volume, whole, `/home/<harness>`. Every harness's own
//! tests pin these lines from here. Each host is cited, to its vendor's own documentation or, where that names a
//! wildcard or nothing, to the vendor's own source or build, in its harness's record's "Hosts by name"
//! (docs/research/s19-claude-agentfile to s28-pi-agentfile; step 18's part 4).

use super::ALL;

/// Each built-in's harness and the hosts it reaches by name, as its NETWORK line names them (sorted, once
/// each), and nothing else ([`destinations`]): each list its harness's record's, which the comment above it names.
const HOSTS: &[(&str, &[&str])] = &[
    // docs/research/s19-claude-agentfile, "Hosts by name": Anthropic's network docs.
    ("claude", &["api.anthropic.com:443", "claude.ai:443", "platform.claude.com:443"]),
    // docs/research/s20-codex-agentfile, "Hosts by name": OpenAI's pages and codex-rs rust-v0.167.2 (its docs list no hosts).
    ("codex", &["api.openai.com:443", "auth.openai.com:443", "chatgpt.com:443"]),
    // docs/research/s21-gemini-agentfile, "Hosts by name": Google's docs and gemini-cli v0.68.0's own sources.
    ("gemini", &["cloudcode-pa.googleapis.com:443", "generativelanguage.googleapis.com:443", "oauth2.googleapis.com:443", "www.googleapis.com:443"]),
    // docs/research/s26-agy-agentfile, "Hosts by name": Antigravity's and Google's docs and agy 1.5.7's build; its four sign-ins, ROOT's call (destinations::BY_USE).
    ("antigravity", &[
        "aiplatform.eu.rep.googleapis.com:443", "aiplatform.googleapis.com:443", "aiplatform.us.rep.googleapis.com:443",
        "businessaicode.eu.rep.googleapis.com:443", "businessaicode.googleapis.com:443", "businessaicode.us.rep.googleapis.com:443",
        "cloudcode-pa.googleapis.com:443", "discoveryengine.googleapis.com:443", "eu-discoveryengine.googleapis.com:443",
        "generativelanguage.googleapis.com:443", "oauth2.googleapis.com:443", "sts.googleapis.com:443", "us-discoveryengine.googleapis.com:443"
    ]),
    // docs/research/s27-cursor-agentfile, "Hosts by name": Cursor's network docs, and its CLI's own code for api2direct (in *.cursor.sh).
    ("cursor", &[
        "agent.api5.cursor.sh:443", "agent.global.api5.cursor.sh:443", "agent.us.api5.cursor.sh:443", "agentn.api5.cursor.sh:443",
        "agentn.global.api5.cursor.sh:443", "agentn.us.api5.cursor.sh:443", "api2.cursor.sh:443", "api2direct.cursor.sh:443", "api5.cursor.sh:443",
        "repo42.cursor.sh:443"
    ]),
    // docs/research/s25-devin-agentfile, "Hosts by name": Devin's allowlist, and its build for the hosts in its .codeium.com.
    ("devin", &["api.devin.ai:443", "server.codeium.com:443", "unleash.codeium.com:443"]),
    // docs/research/s24-amp-agentfile, "Hosts by name": Amp's docs and its own build.
    ("amp", &["ampcode.com:443"]),
    // docs/research/s23-goose-agentfile, "Hosts by name" and evidence/hosts/hosts.tsv: every provider Goose 1.59.0 reaches (ROOT's call).
    ("goose", &[
        "ai-gateway.vercel.sh:443", "ai.salad.cloud:443", "aihubmix.com:443", "api.aigateway.sh:443", "api.aimlapi.com:443", "api.anthropic.com:443",
        "api.avian.io:443", "api.cerebras.ai:443", "api.deepseek.com:443", "api.empiriolabs.ai:443", "api.eurouter.ai:443", "api.fireworks.ai:443",
        "api.friendli.ai:443", "api.github.com:443", "api.githubcopilot.com:443", "api.gondola-ai.com:443", "api.groq.com:443", "api.inceptionlabs.ai:443",
        "api.intelligence.io.solutions:443", "api.kimi.com:443", "api.meta.ai:443", "api.minimax.io:443", "api.mistral.ai:443", "api.moonshot.cn:443",
        "api.novita.ai:443", "api.openai.com:443", "api.opper.ai:443", "api.orcarouter.ai:443", "api.perplexity.ai:443", "api.router.tetrate.ai:443",
        "api.routerplex.com:443", "api.routstr.com:443", "api.sakana.ai:443", "api.saygm.com:443", "api.scaleway.ai:443", "api.tensorix.ai:443",
        "api.together.xyz:443", "api.trustedrouter.com:443", "api.typesafe.ai:443", "api.venice.ai:443", "api.x.ai:443", "api.z.ai:443", "auth.kimi.com:443",
        "auth.meta.com:443", "auth.openai.com:443", "auth.x.ai:443", "chatgpt.com:443", "cloud-api.near.ai:443", "cloudcode-pa.googleapis.com:443",
        "dashscope-intl.aliyuncs.com:443", "futurmix.ai:443", "generativelanguage.googleapis.com:443", "github.com:443", "huggingface.co:443",
        "inference.celeris.ai:443", "integrate.api.nvidia.com:443", "maas-token-api.cn-huabei-1.xf-yun.com:443", "nano-gpt.com:443",
        "oai.endpoints.kepler.ai.cloud.ovh.net:443", "oauth2.googleapis.com:443", "ollama.com:443", "open.bigmodel.cn:443", "opencode.ai:443",
        "openrouter.ai:443", "router.huggingface.co:443", "router.pleum.ai:443", "spark-api-open.xf-yun.com:443"
    ]),
    // docs/research/s22-opencode-agentfile, "Hosts by name" and evidence/hosts/hosts.tsv: every provider opencode 2.2.7 reaches (ROOT's call).
    ("opencode", &[
        "agentrouter.org:443", "ai-gateway.helicone.ai:443", "ai-gateway.vercel.sh:443", "ai.zenifra.com:443", "aihubmix.com:443", "aki.io:443",
        "api-gw.klok.ipaas.se:443", "api-inference.modelscope.cn:443", "api-sherlock.cloudferro.com:443", "api.302.ai:443", "api.abliteration.ai:443",
        "api.above.dev:443", "api.ai-router.dev:443", "api.ai21.com:443", "api.aiand.com:443", "api.aimlapi.com:443", "api.aixy-gateway.com:443",
        "api.ambient.xyz:443", "api.anthropic.com:443", "api.anyapi.ai:443", "api.arcee.ai:443", "api.auriko.ai:443", "api.bee.heossi.com:443",
        "api.berget.ai:443", "api.cerebras.ai:443", "api.clarifai.com:443", "api.claudin.io:443", "api.cline.bot:443", "api.cloudflare.com:443",
        "api.code.umans.ai:443", "api.cohere.ai:443", "api.cohere.com:443", "api.commandcode.ai:443", "api.cortecs.ai:443", "api.crossmodel.ai:443",
        "api.deepinfra.com:443", "api.deepseek.com:443", "api.digitalocean.com:443", "api.dinference.com:443", "api.edenai.run:443", "api.empiriolabs.ai:443",
        "api.engy.ai:443", "api.eu.munito.ai:443", "api.fastino.ai:443", "api.fireworks.ai:443", "api.freemodel.dev:443", "api.friendli.ai:443",
        "api.getlilac.com:443", "api.github.com:443", "api.githubcopilot.com:443", "api.gmi-serving.com:443", "api.greenpt.ai:443", "api.groq.com:443",
        "api.hpc-ai.com:443", "api.impossibl.com:443", "api.inceptionlabs.ai:443", "api.inceptron.io:443", "api.inco.ai:443",
        "api.inference.boundless.network:443", "api.inference.crusoecloud.com:443", "api.inference.wandb.ai:443", "api.intelligence.io.solutions:443",
        "api.iteracompute.com:443", "api.jalapeno-cloud.ai:443", "api.jiekou.ai:443", "api.kilo.ai:443", "api.kimi.ai:443", "api.kimi.com:443",
        "api.koscompute.com:443", "api.lab.vispark.in:443", "api.lkeap.cloud.tencent.com:443", "api.llmgateway.io:443", "api.llmtech.eu:443",
        "api.longcat.chat:443", "api.lowrouter.ai:443", "api.lucidquery.com:443", "api.meganova.ai:443", "api.melious.ai:443", "api.meta.ai:443",
        "api.minimax.cn:443", "api.minimax.io:443", "api.mistral.ai:443", "api.modeloracle.com:443", "api.modular.com:443", "api.moonshot.ai:443",
        "api.moonshot.cn:443", "api.morphllm.com:443", "api.munito.ai:443", "api.nan.builders:443", "api.neuralwatt.com:443", "api.nova.amazon.com:443",
        "api.novita.ai:443", "api.ofox.ai:443", "api.openai-compat.model-serving.eu01.onstackit.cloud:443", "api.openai.com:443", "api.openreason.app:443",
        "api.opper.ai:443", "api.orcarouter.ai:443", "api.paretoinference.com:443", "api.pendra.ai:443", "api.perplexity.ai:443", "api.pinference.ai:443",
        "api.poe.com:443", "api.qhaigc.net:443", "api.qnaigc.com:443", "api.reflection.ai:443", "api.regolo.ai:443", "api.routing.run:443",
        "api.runinfra.ai:443", "api.sakana.ai:443", "api.sambanova.ai:443", "api.sarvam.ai:443", "api.scaleway.ai:443", "api.scnet.cn:443", "api.scx.ai:443",
        "api.siliconflow.cn:443", "api.siliconflow.com:443", "api.stdcmpt.com:443", "api.stepfun.ai:443", "api.stepfun.com:443", "api.subconscious.dev:443",
        "api.synthetic.new:443", "api.tbox.cn:443", "api.temprhq.io:443", "api.tensorx.ai:443", "api.thegrid.ai:443", "api.together.xyz:443",
        "api.tokenfactory.nebius.com:443", "api.tokengo.com:443", "api.tokenrouter.com:443", "api.trustedrouter.com:443", "api.unorouter.com:443",
        "api.upstage.ai:443", "api.venice.ai:443", "api.vivgrid.com:443", "api.vultrinference.com:443", "api.wallabytoken.com:443", "api.withhopper.com:443",
        "api.x.ai:443", "api.xiaomimimo.com:443", "api.z.ai:443", "api.zeldoc.ai:443", "apihub.agnes-ai.com:443", "app.frogbot.ai:443",
        "ark.ap-southeast.bytepluses.com:443", "ark.cn-beijing.volces.com:443", "auth.openai.com:443", "auth.x.ai:443", "cc.freemodel.dev:443",
        "chat.d.run:443", "chatgpt.com:443", "cloud-api.near.ai:443", "coding-intl.dashscope.aliyuncs.com:443", "coding-plan-endpoint.kuaecloud.net:443",
        "coding.dashscope.aliyuncs.com:443", "daoxe.com:443", "dashscope-intl.aliyuncs.com:443", "dashscope.aliyuncs.com:443", "developer.amd.com.cn:443",
        "echo.tracerml.ai:443", "gateway.ai.cloudflare.com:443", "generativelanguage.googleapis.com:443", "github.com:443", "gitlab.com:443",
        "go.fastrouter.ai:443", "hyper.charm.land:443", "infer.flow7.org:443", "inference.baseten.co:443", "inference.coralbricks.ai:443",
        "inference.do-ai.run:443", "inference.generativeai.us-chicago-1.oci.oraclecloud.com:443", "inference.hetzner.com:443", "inference.net:443",
        "inference.poolside.ai:443", "inference.runanywhere.ai:443", "inference.tinfoil.sh:443", "inference.us-west.modal.direct:443",
        "integrate.api.nvidia.com:443", "kenari.id:443", "llm.chutes.ai:443", "llm.submodel.ai:443", "llmtr.com:443", "maas-api.ebcloud.com:443",
        "microquickjs.com:443", "moark.com:443", "model.inferx.net:443", "modelishub.com:443", "models.mixlayer.ai:443", "models.opencode.ai:443",
        "models.think.evroc.com:443", "nano-gpt.com:443", "oai.endpoints.kepler.ai.cloud.ovh.net:443", "ollama.com:443", "open.bigmodel.cn:443",
        "openai.blueclaw.network:443", "openai.bothub.ru:443", "opencode.ai:443", "openrouter.ai:443", "pass.wafer.ai:443",
        "release-assets.githubusercontent.com:443", "routellm.abacus.ai:443", "router.huggingface.co:443", "router.inference.ai.exchange:443",
        "router.neosmith.ai:443", "router.requesty.ai:443", "tinker.thinkingmachines.dev:443", "token-plan-ams.xiaomimimo.com:443",
        "token-plan-cn.xiaomimimo.com:443", "token-plan-sgp.xiaomimimo.com:443", "token-plan.ap-southeast-1.maas.aliyuncs.com:443",
        "token-plan.cn-beijing.maas.aliyuncs.com:443", "token-plan.maas.qianwenaiapi.com:443", "token-plan.maas.qwencloudapi.com:443",
        "token.sensenova.cn:443", "tokenhub.tencentmaas.com:443", "vancine.com:443", "www.xpersona.co:443", "zenmux.ai:443"
    ]),
    // docs/research/s28-pi-agentfile, "Hosts by name" and evidence/hosts/hosts.tsv: every provider pi 1.5.2 reaches (ROOT's call).
    ("pi", &[
        "ai-gateway.vercel.sh:443", "api.ant-ling.com:443", "api.anthropic.com:443", "api.cerebras.ai:443", "api.cloudflare.com:443", "api.deepseek.com:443",
        "api.fireworks.ai:443", "api.github.com:443", "api.groq.com:443", "api.individual.githubcopilot.com:443", "api.kimi.com:443", "api.meta.ai:443",
        "api.minimax.io:443", "api.minimaxi.com:443", "api.mistral.ai:443", "api.moonshot.ai:443", "api.moonshot.cn:443", "api.openai.com:443",
        "api.together.ai:443", "api.typesafe.ai:443", "api.x.ai:443", "api.xiaomimimo.com:443", "api.z.ai:443", "auth.kimi.com:443", "auth.meta.com:443",
        "auth.openai.com:443", "auth.x.ai:443", "chatgpt.com:443", "gateway.ai.cloudflare.com:443", "generativelanguage.googleapis.com:443", "github.com:443",
        "inference.baseten.co:443", "integrate.api.nvidia.com:443", "open.bigmodel.cn:443", "opencode.ai:443", "openrouter.ai:443", "pi.dev:443",
        "platform.claude.com:443", "radius.pi.dev:443", "router.huggingface.co:443", "token-plan-ams.xiaomimimo.com:443", "token-plan-cn.xiaomimimo.com:443",
        "token-plan-sgp.xiaomimimo.com:443", "token-plan.ap-southeast-1.maas.aliyuncs.com:443", "token-plan.cn-beijing.maas.aliyuncs.com:443"
    ]),
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

/// One built-in's [`super::destinations::BY_USE`] table.
fn by_use(harness: &str) -> anyhow::Result<&'static super::destinations::ByUse> {
    super::destinations::BY_USE.iter().find(|b| b.harness == harness).ok_or_else(|| anyhow::anyhow!("no hosts by use for {harness}"))
}

/// What breaks a built-in's hosts by use: its rows and what every run keeps, together, aren't its image's hosts; a row
/// is empty, unsorted or names a host twice; or two rows share an id.
fn by_use_problems(harness: &str, always: &[&str], by: &[(&str, &[&str])]) -> Vec<String> {
    let mut found = Vec::new();
    let mut all: Vec<&str> = always.iter().copied().chain(by.iter().flat_map(|(_, h)| h.iter().copied())).collect();
    all.sort_unstable();
    all.dedup();
    let image = hosts(harness);
    if all != image {
        found.push(format!("together {all:?}, not the image's {image:?}"));
    }
    let once = |h: &[&str]| h.is_sorted() && h.windows(2).all(|w| w.first() != w.get(1));
    if !once(always) {
        found.push(format!("what every run keeps, {always:?}, isn't sorted once each"));
    }
    for (id, hosts) in by {
        if hosts.is_empty() || !once(hosts) {
            found.push(format!("{id}: {hosts:?} isn't sorted once each"));
        }
    }
    let mut ids: Vec<&str> = by.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w.first() == w.get(1)) {
        found.push(format!("an id twice among {ids:?}"));
    }
    found
}

/// Each built-in that serves more than one provider or sign-in has its hosts by use (`destinations::BY_USE`, what
/// kiln's launch is to keep a run to, plan step 17), and its rows with what every run keeps are its image's hosts
/// together, each row sorted once each and each id once. A row with a host fewer, a host its image doesn't name, or an
/// id twice is caught.
#[test]
fn step18_each_builtins_hosts_by_use_are_its_image_together() -> anyhow::Result<()> {
    let tables = super::destinations::BY_USE;
    let mut harnesses: Vec<&str> = tables.iter().map(|b| b.harness).collect();
    harnesses.sort_unstable();
    anyhow::ensure!(harnesses == ["antigravity", "goose", "opencode", "pi"], "hosts by use for {harnesses:?}");
    for b in tables {
        let found = by_use_problems(b.harness, b.always, b.by);
        anyhow::ensure!(found.is_empty(), "{}: {found:?}", b.harness);
    }
    let pi = by_use("pi")?;
    let rows: Vec<(&str, Vec<&str>)> = pi.by.iter().map(|(id, h)| (*id, h.to_vec())).collect();
    let fewer: Vec<(&str, Vec<&str>)> = rows.iter().map(|(id, h)| (*id, h.iter().copied().filter(|x| !(*id == "groq" && *x == "api.groq.com:443")).collect())).collect();
    let more: Vec<(&str, Vec<&str>)> = rows.iter().map(|(id, h)| (*id, h.iter().copied().chain((*id == "groq").then_some("example.com:443")).collect())).collect();
    let twice: Vec<(&str, Vec<&str>)> = rows.iter().cloned().chain(rows.first().cloned()).collect();
    for (what, bent) in [("a host fewer", fewer), ("a host more", more), ("an id twice", twice)] {
        let bent: Vec<(&str, &[&str])> = bent.iter().map(|(id, h)| (*id, h.as_slice())).collect();
        anyhow::ensure!(!by_use_problems("pi", pi.always, &bent).is_empty(), "{what} passed");
    }
    Ok(())
}

/// Antigravity's sign-ins (`destinations::BY_USE`'s rows for it) are its image's hosts together; none is an mTLS twin,
/// which a client uses only with a device certificate (Google's AIP-4114) and no microVM holds; every enterprise
/// location keeps the global endpoints its sign-in asks (enterprise.md: `businessaicode` and `cloudcode-pa`); and a
/// personal account's or a key's sign-in takes no enterprise host. Bent sets are caught.
#[test]
fn step18_agy_sign_ins_name_the_image_together_and_no_mtls_host() -> anyhow::Result<()> {
    fn problems(sign_ins: &[(&str, &[&str])]) -> Vec<String> {
        let mut found = by_use_problems("antigravity", &[], sign_ins);
        let enterprise = |h: &str| ["businessaicode", "aiplatform", "discoveryengine"].iter().any(|s| h.contains(s));
        for (name, hosts) in sign_ins {
            if let Some(h) = hosts.iter().find(|h| h.contains(".mtls.")) {
                found.push(format!("{name}: {h}, an mTLS twin"));
            }
            if name.starts_with("enterprise-") && !["businessaicode.googleapis.com:443", "cloudcode-pa.googleapis.com:443"].iter().all(|g| hosts.contains(g)) {
                found.push(format!("{name}: without the global endpoints its sign-in asks"));
            }
            if ["google-account", "gemini-api-key"].contains(name) && hosts.iter().any(|h| enterprise(h)) {
                found.push(format!("{name}: an enterprise host"));
            }
        }
        found
    }
    let agy = by_use("antigravity")?;
    anyhow::ensure!(agy.always.is_empty(), "agy keeps {:?} whatever its sign-in", agy.always);
    anyhow::ensure!(problems(agy.by).is_empty(), "{:?}", problems(agy.by));
    let without = |name: &str, host: &str| -> Vec<(String, Vec<&'static str>)> {
        agy.by.iter().map(|(n, h)| ((*n).to_owned(), h.iter().copied().filter(|x| !(*n == name && *x == host)).collect())).collect()
    };
    let with = |name: &str, host: &'static str| -> Vec<(String, Vec<&'static str>)> {
        agy.by.iter().map(|(n, h)| ((*n).to_owned(), h.iter().copied().chain((*n == name).then_some(host)).collect())).collect()
    };
    let bent = [
        ("a host fewer", without("gemini-api-key", "generativelanguage.googleapis.com:443")),
        ("an mTLS twin", with("enterprise-global", "businessaicode.mtls.googleapis.com:443")),
        ("a location without its global endpoint", without("enterprise-us", "businessaicode.googleapis.com:443")),
        ("a personal account given an enterprise host", with("google-account", "discoveryengine.googleapis.com:443")),
    ];
    for (what, sets) in bent {
        let sets: Vec<(&str, &[&str])> = sets.iter().map(|(n, h)| (n.as_str(), h.as_slice())).collect();
        anyhow::ensure!(!problems(&sets).is_empty(), "{what} passed");
    }
    Ok(())
}
