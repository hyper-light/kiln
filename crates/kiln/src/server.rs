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
    let cfg = Arc::new(cfg);
    std::fs::create_dir_all(&cfg.server.data_dir)
        .with_context(|| format!("create data dir {}", cfg.server.data_dir.display()))?;
    let store = crate::store::Store::open(&cfg.data("kiln.db"))?;
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
        leaf,
        server: true,
        dial_deadline: crate::net::DIAL_DEADLINE,
    })?;
    let hub = Hub::new(cfg.clone(), store, logs, net, pki.clone(), admin, join, shutdown.clone());
    // Process restarts by the supervisor count as restarts too (generation 1 = first start).
    crate::metrics::global().supervisor_restarts.add(hub.generation.saturating_sub(1));

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
    child!("sampler", h => h.sampler_loop());
    child!("retention", h => h.retention_loop());
    child!("gitops-discover", h => crate::gitops::discover_loop(h));
    child!("gitops-poll", h => crate::gitops::poll_loop(h));
    child!("gitops-schedule", h => crate::gitops::schedule_loop(h));

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
    } else {
        let _ = hub.k8s.set(None);
    }

    // HTTP.
    {
        let h = hub.clone();
        let sd = shutdown.clone();
        sup::spawn(reg, "http", shutdown.clone(), move || {
            let hub = h.clone();
            let sd = sd.clone();
            async move {
                let addr: SocketAddr = hub.cfg.server.http_addr.parse().with_context(|| format!("http_addr {}", hub.cfg.server.http_addr))?;
                // Bound through std (SO_REUSEADDR: a restart rebinds at once despite TIME_WAIT,
                // and a second server on the same port fails loudly), then adopted by hyper-rt.
                let std_listener = std::net::TcpListener::bind(addr).with_context(|| format!("bind {addr}"))?;
                let listener = hyper_rt::tcp::TcpListener::from_owned(std::os::fd::OwnedFd::from(std_listener), 128)
                    .map_err(|e| anyhow::anyhow!("adopt listener {addr}: {e}"))?;
                tracing::info!(%addr, url = %hub.cfg.server.public_url, "HTTP listening (hyper-rt)");
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
                            crate::rt::spawn_spread(async move {
                                crate::http::server::serve_connection(crate::http::io::RtIo::new(stream), api).await;
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
            shell: cfg.terminal.shell.clone(),
            credentials: Some(creds),
        };
        sup::spawn(reg, "local-agent", shutdown.clone(), move || crate::agent::run(opts.clone(), shutdown_child()));
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data = %cfg.server.data_dir.display(),
        "kiln server started"
    );
    if !std::env::var("KILN_SUPERVISED").is_ok_and(|v| v == "1") {
        tracing::info!("tip: run under `kiln supervise -- server` (or `kiln install`) for automatic restarts");
    }
    shutdown.cancelled().await;
    tracing::info!("shutting down");
    crate::rt::sleep(Duration::from_millis(300)).await;
    Ok(())
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
