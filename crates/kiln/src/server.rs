//! `kiln server`: wires every subsystem together under in-process supervision.
//!
//! ```text
//!  supervised children        restart on error/panic with backoff
//!  ├─ http          hyper on hyper-rt: UI, API, websockets, /metrics
//!  ├─ net           QUIC events → hub
//!  ├─ scheduler     queue → agents / pods, timeouts, lost agents
//!  ├─ gitops-*      discovery, polling, schedules
//!  ├─ k8s-reaper    pod lifecycle (Kubernetes mode)
//!  ├─ sampler       server host metrics
//!  ├─ retention     prune old runs + logs
//!  ├─ discovery     multicast DNS: announce `_kiln._tcp.local.`, browse for other kiln instances
//!  └─ local-agent   in-process agent over loopback QUIC (bare-metal mode)
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use futures::FutureExt as _;

use crate::rt::Shutdown;

use crate::config::Config;
use crate::hub::Hub;
use crate::sup::{self, Registry};

/// A runner seen this recently before the server started counts as connected before it.
const RUNNERS_BEFORE_S: i64 = 900;

pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Registry::default)
}

fn host_of(addr: &str) -> Option<String> {
    let s = addr.split("://").last()?.split('/').next()?;
    let host = s.rsplit_once(':').map_or(s, |(h, _)| h).trim_matches(|c| c == '[' || c == ']');
    (!host.is_empty()).then(|| host.to_owned())
}

pub fn admin_token(cfg: &Config) -> anyhow::Result<String> {
    if let Ok(t) = std::env::var("KILN_ADMIN_TOKEN")
        && !t.trim().is_empty() {
            return Ok(t.trim().to_owned());
        }
    let path = cfg.server.admin_token_file.clone().unwrap_or_else(|| cfg.data("admin.token"));
    crate::util::load_or_create_token(&path).with_context(|| format!("admin token at {}", path.display()))
}

pub fn token_path(cfg: &Config) -> PathBuf {
    cfg.server.admin_token_file.clone().unwrap_or_else(|| cfg.data("admin.token"))
}

pub async fn run(cfg: Config, shutdown: Shutdown) -> anyhow::Result<()> {
    crate::os::start_reaper();
    crate::updates::warm_own_binary().await;
    let cfg = Arc::new(cfg);
    std::fs::create_dir_all(&cfg.server.data_dir)
        .with_context(|| format!("create data dir {}", cfg.server.data_dir.display()))?;
    // Earlier builds' UI assets stay servable (pages loaded before this deploy); in the background.
    {
        let dir = cfg.server.data_dir.clone();
        crate::rt::spawn_spread(async move {
            let _ = crate::rt::blocking(move || crate::ui::keep_assets(&dir)).await;
        });
    }
    crate::password::seed_from_env(&crate::password::path(&cfg));
    let store = if cfg.cluster.group { crate::store::Store::open_grouped(&cfg.data("kiln.db"), &cfg.data("cluster"))? } else { crate::store::Store::open(&cfg.data("kiln.db"))? };
    tracing::info!(group = store.grouped(), "the store's writes go through the cluster group (docs/p2p.md P1)");
    let runners_before = store.runners_before(RUNNERS_BEFORE_S).await.unwrap_or(0);
    let requeued = store.recover().await?;
    if requeued > 0 {
        tracing::warn!(jobs = requeued, "jobs were running at last shutdown; waiting for their agents to reclaim them");
    }
    let logs = crate::logs::Logs::new(cfg.data("logs"), cfg.retention.max_job_log_mb.saturating_mul(1 << 20));
    let pki = Arc::new(crate::pki::Pki::open(&cfg.data("pki"))?);
    let mut names = cfg.server.tls_names.clone();
    names.extend(host_of(&cfg.server.public_url));
    names.extend(host_of(&cfg.server.quic_advertise));
    if let Some(n) = &cfg.kubernetes.server_quic {
        names.extend(host_of(n));
    }
    names.retain(|n| n != "0.0.0.0");
    names.dedup();
    let leaf = pki.server_leaf(&names)?;
    let admin = admin_token(&cfg)?;
    let join = crate::util::load_or_create_token(&cfg.data("join.token"))?;

    let quic_bind: SocketAddr = cfg.server.quic_addr.parse().with_context(|| format!("quic_addr {}", cfg.server.quic_addr))?;
    let (net, net_events) = crate::net::start(crate::net::Spec {
        bind: quic_bind,
        ca_der: pki.ca_der.clone(),
        leaf: leaf.clone(),
        server: true,
        dial_deadline: crate::net::DIAL_DEADLINE,
    })?;
    // A test build's faults (`KILN_CHAOS`): frames lost, sent twice or sent late below the channel,
    // which it makes up for.
    if let Some(c) = crate::runner::chaos::get() {
        net.command(crate::net::Out::Seed(c.seed)).await;
        for rule in c.faults.iter().copied() {
            net.command(crate::net::Out::Shed(rule)).await;
        }
    }
    // HTTPS on the HTTP port, when asked: the configured certificate, else the kiln CA's.
    let tls = if cfg.server.https { Some(server_tls(&cfg, &leaf)?) } else { None };
    let hub = Hub::new(cfg.clone(), store, logs, net, pki.clone(), admin, join, shutdown.clone())?;
    hub.runners_before.store(runners_before, std::sync::atomic::Ordering::Relaxed);
    // Where kiln keeps its bytes (docs/blobs.md): what was kept before taken in, what's kept read.
    crate::blobs::start(&hub).await;
    // The CI engine (docs/ci.md), when turned on.
    crate::ci::dispatch::spawn(hub.clone());
    // Terminal sessions' names, kept across restarts.
    hub.terminals.keep_names_in(cfg.server.data_dir.join("terminal-names.json"));
    // Runners' front doors: what the UI set, sent to each runner when it connects.
    hub.front_doors.load(&hub.store).await?;
    // How many agent sessions each machine runs at once, as the UI set it: sent to each runner as
    // it connects, and the server's own agents held to theirs from the start.
    hub.agent_limits.load(&hub.store).await?;
    crate::harness::limit::hold_local(&hub);
    // Peered clusters (docs/peers.md): forwarded to, and their ports open, from the start.
    crate::peers::start(&hub).await;
    // The tailnet's DNS for them: the Tailscale credential given in the UI, the routes kiln set.
    hub.tailnet.load(&hub.cfg, &hub.store).await;
    // Process restarts by the supervisor count as restarts too (generation 1 = first start).
    crate::metrics::global().supervisor_restarts.add(hub.generation.saturating_sub(1));

    // The copies of agent sessions its store has, rebuilt before any runner's message is taken
    // in: a runner that comes back after this server started again finds them, and sends only
    // what they lack (docs/wire.md, "Restarts").
    if !cfg.ci.local {
        crate::harness::registry::Registry::list_away(&hub).await;
    }

    // Transport events. Each message is handled in isolation; the stream itself never ends while
    // the net thread lives (it restarts its endpoint internally).
    {
        let hub = hub.clone();
        sup::spawn_guarded("net-events", async move { hub.net_loop(net_events).await });
    }

    let reg = registry();
    macro_rules! child {
        ($name:literal, $hub:ident => $body:expr) => {{
            let h = hub.clone();
            sup::spawn(reg, $name, shutdown.clone(), move || {
                let $hub = h.clone();
                async move { $body.await }
            });
        }};
    }
    child!("scheduler", h => h.scheduler_loop());
    child!("commands", h => h.commands_loop());
    child!("blobs", h => crate::blobs::run(h));
    child!("transcripts", h => crate::harness::archive::run(h));
    // `kiln workflow run`'s own server (docs/ci.md W17-R06) runs CI and nothing else: no polling
    // or schedules, terminals, agents, sync, fleet or UI.
    if !cfg.ci.local {
        // Agents kiln starts on this machine get its MCP server and skills (each agent a token of
        // its own: docs/agents.md, "Agents that run agents").
        let port = cfg.server.http_addr.rsplit_once(':').map(|(_, p)| p.to_owned()).unwrap_or_else(|| "7070".into());
        if let Err(e) = crate::mcp::init_kit(&hub, &format!("http://127.0.0.1:{port}")) {
            tracing::warn!(error = %format!("{e:#}"), "agents won't be given kiln's MCP server and skills");
        }
        child!("sampler", h => h.sampler_loop());
        child!("conversation-stores", h => crate::harness::stores::run(h));
        child!("retention", h => h.retention_loop());
        child!("terminals", h => h.terminal_loop());
        child!("agent-screens", h => crate::harness::screen::watch(h));
        child!("goal-stacks", h => crate::goals::run(h));
        child!("plan-runs", h => crate::plans::agents::drive(h));
        child!("harness", h => crate::harness::registry::Registry::run_local(h));
        child!("push", h => crate::push::run(h));
        child!("gitops-discover", h => crate::gitops::discover_loop(h));
        child!("gitops-poll", h => crate::gitops::poll_loop(h));
        child!("gitops-schedule", h => crate::gitops::schedule_loop(h));
        child!("land-queue", h => crate::ci::land::run_loop(h));
        child!("github-checks", h => crate::githost::github::checks_loop(h));
        child!("host-statuses", h => crate::githost::statuses::statuses_loop(h));
        child!("ci-providers", h => crate::ci::providers::sync_loop(h));
        child!("pool-samples", h => crate::ci::pools::sample_loop(h));
        child!("diff-runs", h => crate::ci::review::runs_loop(h));
        child!("agent-changes", h => crate::workspace::follow::follow_loop(h));
        child!("attachments", h => crate::ci::attach::run(h));
        child!("previews", h => crate::ci::previews::run(h));
        child!("review-replies", h => crate::ci::replies::run(h));
        // Bisections and experiments this server was driving when it stopped stopped with it
        // (docs/ci.md W20).
        let h = hub.clone();
        crate::rt::spawn(async move {
            crate::ci::bisect::interrupted(&h).await;
            crate::ci::experiments::interrupted(&h).await;
        });
        child!("registry-collect", h => crate::ci::registry::collect_loop(h));
        // kiln's base chart in its own registry, for apps' charts to depend on (docs/deploy-api.md §3.4).
        child!("kiln-base", h => crate::deploy::init::publish_base(h));
        if cfg.cd.enabled {
            child!("cd", h => crate::cd::run(h));
            child!("deploy", h => crate::deploy::run(h));
        }
        child!("sync", h => h.sync_loop());
        child!("cluster", h => h.cluster_loop());
        child!("tailnet-dns", h => crate::frontdoor::tailnet::run(h));
        child!("in-step", h => crate::updates::in_step_loop(h));
        child!("ui-server", h => crate::uiserver::run(h));
        child!("fleet", h => crate::fleet::follow_loop(h));
        // WebRTC to streams' viewers, on its own UDP port (docs/previews.md, "Streams").
        if cfg.server.webrtc_addr.as_deref().is_some_and(|a| !a.trim().is_empty()) {
            child!("webrtc", h => crate::harness::webrtc::run(h));
        }
    }
    // Observe (docs/observe.md): not in `kiln workflow run`'s own server, which runs CI alone.
    if cfg.observe.enabled && !cfg.ci.local {
        let days = |d: u32| std::time::Duration::from_secs(u64::from(d.max(1)) * 86_400);
        let obs = crate::observe::Observe::start(crate::observe::Settings {
            dir: cfg.data("observe"),
            role: crate::observe::Role::Server { key: hub.admin_token.clone() },
            metrics_retention: days(cfg.observe.metrics_retention_days),
            logs_retention: days(cfg.observe.logs_retention_days),
            spans_retention: days(cfg.observe.spans_retention_days),
            max_series: cfg.observe.max_series,
        });
        crate::observe::host::start(obs.clone(), crate::os::hostname(), "server", std::time::Duration::from_secs(u64::from(cfg.observe.host_seconds)));
        crate::observe::containers::start(obs.clone(), crate::os::hostname(), std::time::Duration::from_secs(u64::from(cfg.observe.host_seconds)));
        crate::observe::indexes::start_usage(obs.clone(), crate::os::hostname(), crate::observe::indexes::USAGE_EVERY);
        crate::observe::kiln::start_registry(obs.clone(), crate::os::hostname(), std::time::Duration::from_secs(u64::from(cfg.observe.host_seconds)), Some(hub.net.clone()));
        if let Some(n) = cfg.observe.query_helpers {
            kiln_obs::query::eval::set_helpers(n as usize);
        }
        // A query's helpers yield to the writes the server takes (§12.19).
        kiln_obs::query::eval::on_helper_start(crate::os::lower_thread_priority);
        crate::observe::api::set_global(obs);
        crate::observe::api::set_dashboards(std::sync::Arc::new(crate::observe::dashboards::Dashboards::open(&cfg.data("observe"))));
        crate::observe::api::set_slos(std::sync::Arc::new(crate::observe::slos::Slos::open(&cfg.data("observe"))));
        let log_metrics = crate::observe::logmetrics::LogMetrics::open(&cfg.data("observe"));
        crate::observe::api::set_log_metrics(log_metrics.clone());
        // Its loop holds nothing that panics (the crate's lints): run, not supervised.
        crate::rt::spawn(log_metrics.run(shutdown.clone()));
        // Metrics from spans, kiln's trace metrics among them (`kiln_obs::apm`): alike.
        let span_metrics = crate::observe::logmetrics::LogMetrics::open_spans(&cfg.data("observe"));
        crate::observe::api::set_span_metrics(span_metrics.clone());
        crate::rt::spawn(span_metrics.run(shutdown.clone()));
        // Error Tracking's issues, from the errors those count (`crate::observe::errors`).
        let issues = crate::observe::errors::Issues::open(&cfg.data("observe"));
        crate::observe::api::set_issues(issues.clone());
        crate::rt::spawn(issues.run(shutdown.clone()));
        child!("observe", h => crate::observe::hub::send_loop(h));
        // Profilers' uploads (`crate::observe::profiles`), kept here alone: opened on a thread of their
        // own (their headers read), taken once it has.
        crate::observe::profiles::start(
            cfg.data("observe").join("profiles"),
            i64::from(cfg.observe.profiles_retention_days.max(1)) * 86_400_000,
            cfg.observe.profiles_max_gb.map(|g| u64::from(g) << 30),
        );
        // Incidents (`crate::observe::incidents`), told to their handles through the monitors'
        // integrations.
        let incidents = crate::observe::incidents::Incidents::open(&cfg.data("observe"));
        crate::observe::incidents::set(incidents.clone());
        // Notebooks (`crate::observe::notebooks`), postmortems among them, linked at the server's
        // public URL.
        crate::observe::notebooks::set(crate::observe::notebooks::Notebooks::open(&cfg.data("observe"), &cfg.server.public_url));
        match crate::http::client::Client::public() {
            Ok(http) => {
                let ms = crate::observe::monitors::Monitors::open(&cfg.data("observe"), std::sync::Arc::new(http), &cfg.server.public_url);
                crate::observe::api::set_monitors(ms.clone());
                let told = ms.clone();
                incidents.on_change(move |t| told.tell_incident(&t));
                // Its loop holds nothing that panics (the crate's lints): run, not supervised.
                crate::rt::spawn(ms.run(shutdown.clone()));
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "monitors did not start: no HTTPS client"),
        }
        match crate::http::client::Client::public() {
            Ok(http) => {
                let http = std::sync::Arc::new(http);
                let clouds = crate::observe::cloud::Clouds::open(&cfg.data("observe"), &cfg.data("secrets").join("master.key"), http.clone(), shutdown.clone());
                crate::observe::api::set_clouds(clouds.clone());
                clouds.start();
                // Log archives, written with the cloud integrations' credentials. Its loop holds
                // nothing that panics (the crate's lints): run, not supervised.
                let archives = crate::observe::archives::Archives::open(&cfg.data("observe"), clouds, http);
                crate::observe::api::set_archives(archives.clone());
                crate::rt::spawn(archives.clone().run(shutdown.clone()));
                // Rehydrations from the archives, each read into a view of its own.
                let rehydrations = crate::observe::rehydrate::Rehydrations::open(&cfg.data("observe"), archives);
                crate::observe::api::set_rehydrations(rehydrations.clone());
                crate::rt::spawn(rehydrations.run(shutdown.clone()));
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "cloud integrations did not start: no HTTPS client"),
        }
        // Prometheus's scrapes: the targets kept here, the server's own read here, each runner told
        // its own.
        match crate::http::client::Client::public() {
            Ok(http) => {
                let targets = std::sync::Arc::new(crate::observe::scrape::Targets::open(&cfg.data("observe"), &cfg.data("secrets").join("master.key")));
                let all = targets.list();
                crate::observe::scrape::start(crate::observe::scrape::SERVER, &crate::os::hostname(), std::sync::Arc::new(http), shutdown.clone()).set(&all);
                if let Some(o) = crate::observe::api::get() {
                    o.set_scrapes(all);
                }
                crate::observe::api::set_scrape_targets(targets);
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "scrapes did not start: no HTTPS client"),
        }
        // Synthetic tests: kept here, the server's own run here, each runner told its own.
        let tests = std::sync::Arc::new(crate::observe::synthetics::Tests::open(&cfg.data("observe"), &cfg.data("secrets").join("master.key")));
        let all = tests.list();
        crate::observe::synthetics::start(crate::observe::synthetics::SERVER, shutdown.clone()).set(&all);
        if let Some(o) = crate::observe::api::get() {
            o.set_synthetics(all);
        }
        crate::observe::api::set_synthetics(tests);
        // DogStatsD listeners: kept here, the server's own opened here, each runner told its own.
        let listeners = std::sync::Arc::new(crate::observe::statsd::Listeners::open(&cfg.data("observe")));
        let all = listeners.list();
        crate::observe::statsd::start(crate::observe::statsd::SERVER, &crate::os::hostname(), shutdown.clone()).set(&all);
        if let Some(o) = crate::observe::api::get() {
            o.set_statsd(all);
        }
        crate::observe::api::set_statsd(listeners);
        // Log pipelines: kept here, applied here and by every runner.
        let pipelines = std::sync::Arc::new(crate::observe::pipeline::kept::Pipelines::open(&cfg.data("observe")));
        crate::observe::pipeline::kept::activate(&pipelines.defs());
        if let Some(o) = crate::observe::api::get() {
            o.set_pipelines(pipelines.defs());
        }
        crate::observe::api::set_pipelines(pipelines);
        // Log indexes: kept here, routed by here and by every runner; their quotas counted here.
        let indexes = std::sync::Arc::new(crate::observe::indexes::Indexes::open(&cfg.data("observe")));
        crate::observe::indexes::activate(&indexes.defs());
        if let Some(o) = crate::observe::api::get() {
            o.set_indexes(indexes.defs(), indexes.removed_parts());
        }
        crate::observe::indexes::http::set(indexes);
        // The sensitive data scanner's groups and key: kept here, run here and by every runner.
        let scanning = std::sync::Arc::new(crate::observe::scanner::kept::Groups::open(&cfg.data("observe")));
        let (groups, key) = scanning.defs();
        crate::observe::scanner::kept::activate(&groups, &key);
        if let Some(o) = crate::observe::api::get() {
            o.set_scanner(groups, key);
        }
        crate::observe::api::set_scanner(scanning);
        // DORA's ledger: kiln CD's deployments and those Datadog's API is sent, and failures.
        crate::observe::dora::set(crate::observe::dora::Dora::open(&cfg.data("observe"), Some(hub.clone())));
        // The Service Catalog: definitions kept here, the repositories' read every five minutes.
        let catalog = crate::observe::catalog::Catalog::open(&cfg.data("observe"), Some(hub.clone()));
        crate::observe::catalog::set(catalog.clone());
        crate::rt::spawn(catalog.run(shutdown.clone()));
        let infra = crate::observe::infra::Infra::open(&cfg.data("observe"), Some(hub.clone()));
        crate::observe::infra::set(infra.clone());
        crate::rt::spawn(infra.run(shutdown.clone()));
        // Logs collected: the sources kept here, the server's own read here, each runner told its
        // own.
        let sources = std::sync::Arc::new(crate::observe::collect::Sources::open(&cfg.data("observe")));
        let all = sources.list();
        crate::observe::collect::start(crate::observe::collect::SERVER, &crate::os::hostname(), &cfg.data("observe"), shutdown.clone()).set(&all);
        if let Some(o) = crate::observe::api::get() {
            o.set_log_sources(all);
        }
        crate::observe::api::set_log_sources(sources);
        match crate::util::load_or_create_token(&cfg.data("observe").join("ingest.token")) {
            Ok(t) => crate::observe::api::set_ingest_token(t),
            Err(e) => tracing::warn!(error = %e, "observe's ingest token could not be made: OTLP takes the admin token alone"),
        }
    }
    // Home-network discovery (multicast DNS): announce this server, list the kiln instances nearby.
    if cfg.discovery.enabled && (cfg.discovery.announce || cfg.discovery.browse) {
        child!("discovery", h => crate::discovery::Discovery::serve(h));
    }

    // Kubernetes executor: connect in the background so a slow API server never delays startup.
    if cfg.kubernetes.enabled {
        let h = hub.clone();
        sup::spawn(reg, "k8s", shutdown.clone(), move || {
            let hub = h.clone();
            async move {
                let k = match hub.k8s.get() {
                    Some(Some(k)) => k.clone(),
                    _ => {
                        let k = Arc::new(crate::k8s::K8s::connect(&hub.cfg.kubernetes).await?);
                        tracing::info!(namespace = %hub.cfg.kubernetes.namespace, "kubernetes executor ready");
                        let _ = hub.k8s.set(Some(k.clone()));
                        k
                    }
                };
                k.reap_loop(hub).await
            }
        });
        // The `k8s` pools' executor pods (docs/ci.md W13-R07).
        let h = hub.clone();
        sup::spawn(reg, "k8s-pods", shutdown.clone(), move || crate::ci::pools::reap_pods(h.clone()));
    } else {
        let _ = hub.k8s.set(None);
    }

    // The registry's own addresses (below) use the same TLS.
    let registry_tls = tls.clone();
    // HTTP (and HTTPS on the same port with `server.https`).
    {
        let h = hub.clone();
        let sd = shutdown.clone();
        sup::spawn(reg, "http", shutdown.clone(), move || {
            let hub = h.clone();
            let sd = sd.clone();
            let tls = tls.clone();
            async move {
                let addr: SocketAddr = hub.cfg.server.http_addr.parse().with_context(|| format!("http_addr {}", hub.cfg.server.http_addr))?;
                // Bound through std (SO_REUSEADDR: a restart rebinds at once despite TIME_WAIT,
                // and a second server on the same port fails loudly), then adopted by hyper-rt.
                let std_listener = std::net::TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
                let listener = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(std_listener), 128)
                    .map_err(|e| anyhow::anyhow!("adopt listener {addr}: {e}"))?;
                tracing::info!(%addr, url = %hub.cfg.server.public_url, https = tls.is_some(), "HTTP listening (hyper-rt)");
                let api = Arc::new(crate::api::Api { hub });
                loop {
                    let accepted = futures::select_biased! {
                        () = sd.cancelled().fuse() => return Ok(()),
                        r = listener.accept().fuse() => r,
                    };
                    match accepted {
                        Ok(stream) => {
                            // Connections are spread over every shard; a fresh stream has no armed
                            // waits, so it can move.
                            let api = api.clone();
                            let remote = stream.peer_addr().ok();
                            let tls = tls.clone();
                            crate::rt::spawn_spread(async move {
                                crate::http::server::serve_accepted(stream, api, remote, tls.as_ref()).await;
                            });
                        }
                        Err(e) => {
                            // e.g. EMFILE: back off briefly instead of spinning.
                            tracing::warn!(error = %format!("{e:#}"), "accept failed");
                            crate::rt::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        });
    }

    // kiln's OCI registry on addresses of its own (docs/ci.md W12-R01, R02), beside the main
    // origin's: the registry, and each mirror Docker's `registry-mirrors` pulls from at the root.
    let mut registry_addresses: Vec<(String, Option<String>)> = if cfg.ci.enabled { cfg.ci.registry.listen.iter().map(|l| (l.clone(), None)).collect() } else { Vec::new() };
    if cfg.ci.enabled {
        registry_addresses.extend(cfg.ci.registry.mirrors.iter().filter_map(|m| m.listen.clone().map(|l| (l, Some(m.prefix())))));
    }
    for (listen, prefix) in registry_addresses {
        let h = hub.clone();
        let sd = shutdown.clone();
        let tls = registry_tls.clone();
        sup::spawn(reg, &format!("registry {listen}"), shutdown.clone(), move || {
            let (hub, sd, tls, listen, prefix) = (h.clone(), sd.clone(), tls.clone(), listen.clone(), prefix.clone());
            async move {
                let addr: SocketAddr = listen.parse().with_context(|| format!("the registry's listen {listen}"))?;
                let std_listener = std::net::TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
                let listener = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(std_listener), 128).map_err(|e| anyhow::anyhow!("adopt listener {addr}: {e}"))?;
                tracing::info!(%addr, mirror = prefix.as_deref().unwrap_or("-"), "the registry listening");
                let only = Arc::new(crate::ci::registry::Only { hub, prefix });
                loop {
                    let accepted = futures::select_biased! {
                        () = sd.cancelled().fuse() => return Ok(()),
                        r = listener.accept().fuse() => r,
                    };
                    match accepted {
                        Ok(stream) => {
                            let (only, remote, tls) = (only.clone(), stream.peer_addr().ok(), tls.clone());
                            crate::rt::spawn_spread(async move {
                                crate::http::server::serve_accepted(stream, only, remote, tls.as_ref()).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "the registry's accept failed");
                            crate::rt::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        });
    }

    // kiln's OIDC issuer on addresses of its own (docs/ci.md W10-R12): its two documents and
    // nothing else, so they can face the internet without the rest of kiln.
    let issuer_tls = match (&cfg.ci.oidc.listen_cert, &cfg.ci.oidc.listen_key) {
        (Some(c), Some(k)) => {
            let cert = std::fs::read_to_string(c).with_context(|| format!("[ci.oidc] listen_cert {}", c.display()))?;
            let key = std::fs::read_to_string(k).with_context(|| format!("[ci.oidc] listen_key {}", k.display()))?;
            Some(crate::http::tls::ServerTls::from_pem(&cert, &key).context("[ci.oidc] listen_cert and listen_key")?)
        }
        (None, None) => registry_tls.clone(),
        _ => anyhow::bail!("[ci.oidc] listen_cert and listen_key go together"),
    };
    for listen in cfg.ci.oidc.listen.clone() {
        let h = hub.clone();
        let sd = shutdown.clone();
        let tls = issuer_tls.clone();
        sup::spawn(reg, &format!("issuer {listen}"), shutdown.clone(), move || {
            let (hub, sd, tls, listen) = (h.clone(), sd.clone(), tls.clone(), listen.clone());
            async move {
                let addr: SocketAddr = listen.parse().with_context(|| format!("[ci.oidc] listen {listen}"))?;
                let std_listener = std::net::TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
                let listener = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(std_listener), 128).map_err(|e| anyhow::anyhow!("adopt listener {addr}: {e}"))?;
                tracing::info!(%addr, https = tls.is_some(), "the OIDC issuer listening");
                let only = Arc::new(crate::ci::oidc::Only::new(hub));
                loop {
                    let accepted = futures::select_biased! {
                        () = sd.cancelled().fuse() => return Ok(()),
                        r = listener.accept().fuse() => r,
                    };
                    match accepted {
                        Ok(stream) => {
                            let (only, remote, tls) = (only.clone(), stream.peer_addr().ok(), tls.clone());
                            crate::rt::spawn_spread(async move {
                                crate::http::server::serve_accepted(stream, only, remote, tls.as_ref()).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "the issuer's accept failed");
                            crate::rt::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }
        });
    }

    // In-process bare-metal agent, connected over loopback QUIC like any other agent.
    if cfg.local_agent.enabled {
        let name = crate::pki::sanitize_name(&cfg.local_agent_name());
        let creds = (pki.ca_der.clone(), pki.issue_local(&name)?);
        let http_port = cfg.server.http_addr.rsplit_once(':').map_or("7070", |(_, p)| p).to_owned();
        let opts = crate::agent::Opts {
            server_url: format!("http://127.0.0.1:{http_port}"),
            quic: Some(format!("127.0.0.1:{}", quic_bind.port())),
            name,
            labels: cfg.local_agent.labels.clone(),
            slots: cfg.local_agent.slots,
            work_dir: cfg.local_agent.work_dir.clone().unwrap_or_else(|| cfg.data("work")),
            state_dir: cfg.data("local-agent"),
            join_token: None,
            ephemeral: false,
            dedicated_job: None,
            terminal: false, // the server already offers the "local" terminal
            job_terminal: cfg.terminal.enabled,
            shell: cfg.terminal.shell.clone(),
            credentials: Some(creds),
            instance: None,
            config: None,
            reach: None,
            // The server holds the group's store itself.
            observe: false,
            outpost: false,
            declared: cfg.agent.clone(),
        };
        // `kiln workflow run`'s server stops its runner itself, once the runs it follows ended.
        let agent_down = if cfg.ci.local { shutdown.clone() } else { shutdown_child() };
        sup::spawn(reg, "local-agent", shutdown.clone(), move || crate::agent::run(opts.clone(), agent_down.clone()));
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data = %cfg.server.data_dir.display(),
        "kiln server started"
    );
    if !cfg.ci.local && !std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1") {
        tracing::info!("tip: run under `kiln supervise -- server` (or `kiln install`) for automatic restarts");
    }
    shutdown.cancelled().await;
    tracing::info!("shutting down");
    // Log sources write their places down once their last records are written (a moment).
    if let Some(c) = crate::observe::collect::get().cloned() {
        let _ = crate::rt::blocking(move || c.stop_within(Duration::from_secs(3))).await;
    }
    hub.disconnect_all().await;
    // Sync's activity of the last few seconds, which it saves now and then.
    hub.sync.flush().await;
    // Lets the close frames go out and in-flight responses finish.
    crate::rt::sleep(Duration::from_millis(300)).await;
    Ok(())
}

/// The certificate for `server.https`: `server.https_cert` and `server.https_key` when both are
/// given, else the server's leaf from the kiln CA.
fn server_tls(cfg: &Config, leaf: &crate::pki::Leaf) -> anyhow::Result<crate::http::tls::ServerTls> {
    match (&cfg.server.https_cert, &cfg.server.https_key) {
        (Some(c), Some(k)) => {
            let cert = std::fs::read_to_string(c).with_context(|| format!("server.https_cert {}", c.display()))?;
            let key = std::fs::read_to_string(k).with_context(|| format!("server.https_key {}", k.display()))?;
            crate::http::tls::ServerTls::from_pem(&cert, &key).context("server.https_cert and server.https_key")
        }
        (None, None) => crate::http::tls::ServerTls::from_pem(&leaf.cert_pem, &leaf.key_pem).context("the kiln CA's certificate for HTTPS"),
        _ => anyhow::bail!("server.https_cert and server.https_key go together"),
    }
}

/// The local agent shares the process shutdown token.
fn shutdown_child() -> Shutdown {
    crate::SHUTDOWN.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_of("http://kiln.kiln.svc:7070").as_deref(), Some("kiln.kiln.svc"));
        assert_eq!(host_of("10.0.0.5:7071").as_deref(), Some("10.0.0.5"));
        assert_eq!(host_of("https://ci.example/x").as_deref(), Some("ci.example"));
    }
}
