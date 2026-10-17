//! kiln: a self-hosted GitOps CI/CD job server for the hyper-light stack.

pub mod access;
pub mod agent;
pub mod aisessions;
pub mod api;
pub mod autodetect;
pub mod http;
pub mod kubeapi;
pub mod config;
pub mod discovery;
pub mod dns;
pub mod exec;
pub mod gitops;
pub mod harness;
pub mod host;
pub mod hub;
pub mod install;
pub mod instances;
pub mod join;
pub mod k8s;
pub mod logs;
pub mod metrics;
pub mod net;
pub mod os;
pub mod password;
pub mod pipeline;
pub mod pki;
pub mod proc;
pub mod proto;
pub mod rt;
pub mod server;
pub mod store;
pub mod sup;
pub mod supervise;
pub mod terminal;
pub mod termsess;
pub mod ui;
pub mod upgrade;
pub mod util;

/// Process-wide shutdown signal, set once by `main`.
pub static SHUTDOWN: std::sync::OnceLock<rt::Shutdown> = std::sync::OnceLock::new();
