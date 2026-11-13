//! Agent-specific faults (S8, A4), which no failure-detection study covers (comparison §6):
//!
//! - [`LoopDetector`]: a stuck tool loop, a session repeating a call it already made without making
//!   progress (MAST's step repetition, FM-1.3, 15.7% of the failures it annotated).
//! - [`ProviderHealth`]: provider errors told apart. A rate limit or an exhausted quota is the
//!   account's, never the node's: providers type them (Anthropic's 429 `rate_limit_error`, OpenAI's
//!   429 `credit_balance_exhausted`). Overload, server errors and timeouts are the provider's when a
//!   majority of the nodes calling it see them in a window (Anthropic's 529 "across all users"), and a
//!   node's own path when it fails while its peers' calls to the same provider succeed. Either way the
//!   node grade ([`crate::grade`]) never sees them, so quota and outages suspect no node.

use std::collections::BTreeMap;

use crate::HealthError;
use crate::grade::{MIN_OBSERVATIONS, MIN_PEERS, WINDOW_NS};

/// Times the same call may be made without progress before the session is stuck: the third
/// identical call with nothing learned between is a loop. Chosen, not measured: kiln's recorded agent
/// traces are what would set it (A4); falsified by a healthy session making a third identical call
/// without progress.
pub const REPEAT_LIMIT: u32 = 3;
/// Distinct calls tracked per session between progress events: the memory bound, and itself a
/// verdict, since a session that made this many different calls and learned nothing is stuck too.
pub const MAX_DISTINCT_CALLS: usize = 256;

/// Why a session is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stuck {
    /// The same call (`fingerprint`) made `times` times since the last progress.
    Repeated { fingerprint: u64, times: u32 },
    /// [`MAX_DISTINCT_CALLS`] different calls since the last progress.
    NoProgress { calls: usize },
}

#[derive(Debug, Clone)]
pub struct LoopDetector {
    sessions: BTreeMap<u64, BTreeMap<u64, u32>>,
    max_sessions: usize,
}

impl LoopDetector {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: BTreeMap::new(),
            max_sessions,
        }
    }

    /// `session` made a tool call whose name and arguments hash to `fingerprint`.
    pub fn call(&mut self, session: u64, fingerprint: u64) -> Result<Option<Stuck>, HealthError> {
        if !self.sessions.contains_key(&session) && self.sessions.len() >= self.max_sessions {
            return Err(HealthError::Full {
                what: "watched sessions",
                bound: self.max_sessions,
            });
        }
        let calls = self.sessions.entry(session).or_default();
        if !calls.contains_key(&fingerprint) && calls.len() >= MAX_DISTINCT_CALLS {
            return Ok(Some(Stuck::NoProgress { calls: calls.len() }));
        }
        let times = calls.entry(fingerprint).or_insert(0);
        *times = times.saturating_add(1);
        if *times < REPEAT_LIMIT {
            return Ok(None);
        }
        Ok(Some(Stuck::Repeated {
            fingerprint,
            times: *times,
        }))
    }

    /// `session` made progress: a result it hadn't seen, a change to its work.
    pub fn progress(&mut self, session: u64) {
        if let Some(calls) = self.sessions.get_mut(&session) {
            calls.clear();
        }
    }

    pub fn end(&mut self, session: u64) {
        self.sessions.remove(&session);
    }
}

/// A provider call's typed failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderError {
    /// A rate limit (Anthropic 429 `rate_limit_error`, OpenAI 429 rate limit reached), with its
    /// retry-after when the provider gave one.
    RateLimited {
        retry_after_ns: Option<u64>,
    },
    /// The account's quota or credit is spent (OpenAI 429 `credit_balance_exhausted`; Anthropic's
    /// spend-cap 429, which "keeps failing until access resumes").
    QuotaExhausted,
    /// The provider is overloaded (Anthropic 529 `overloaded_error`, OpenAI 503).
    Overloaded,
    /// Any other 5xx.
    ServerError,
    Timeout,
}

/// Whom a provider error is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Hold work on this account until `until_ns`, or until a call succeeds (`None`).
    Account { account: u64, until_ns: Option<u64> },
    /// The provider is failing for most of its callers: stop placing work that needs it.
    Provider { provider: u64 },
    /// The provider recovered.
    ProviderRecovered { provider: u64 },
    /// This node's path to the provider fails where its peers' don't.
    NodePath { node: u64, provider: u64 },
}

#[derive(Debug, Clone, Copy, Default)]
struct Calls {
    calls: u32,
    errors: u32,
}

#[derive(Debug, Clone, Default)]
struct Window {
    nodes: BTreeMap<u64, Calls>,
}

#[derive(Debug, Clone)]
pub struct ProviderHealth {
    providers: BTreeMap<u64, Window>,
    degraded: BTreeMap<u64, bool>,
    max_providers: usize,
    max_nodes: usize,
    window_start: u64,
}

impl ProviderHealth {
    pub fn new(max_providers: usize, max_nodes: usize, start_ns: u64) -> Self {
        Self {
            providers: BTreeMap::new(),
            degraded: BTreeMap::new(),
            max_providers,
            max_nodes,
            window_start: start_ns,
        }
    }

    pub fn degraded(&self, provider: u64) -> bool {
        self.degraded.get(&provider).copied().unwrap_or(false)
    }

    /// A call from `node` on `account` to `provider` ended at `now_ns` with `outcome`. Appends what it,
    /// and any window it closed, decided.
    pub fn call(
        &mut self,
        provider: u64,
        node: u64,
        account: u64,
        now_ns: u64,
        outcome: Result<(), ProviderError>,
        causes: &mut Vec<Cause>,
    ) -> Result<(), HealthError> {
        self.tick(now_ns, causes)?;
        match outcome {
            Err(ProviderError::RateLimited { retry_after_ns }) => {
                causes.push(Cause::Account {
                    account,
                    until_ns: retry_after_ns.map(|d| now_ns.saturating_add(d)),
                });
                return Ok(());
            }
            Err(ProviderError::QuotaExhausted) => {
                causes.push(Cause::Account {
                    account,
                    until_ns: None,
                });
                return Ok(());
            }
            _ => {}
        }
        if !self.providers.contains_key(&provider) && self.providers.len() >= self.max_providers {
            return Err(HealthError::Full {
                what: "watched providers",
                bound: self.max_providers,
            });
        }
        let window = self.providers.entry(provider).or_default();
        if !window.nodes.contains_key(&node) && window.nodes.len() >= self.max_nodes {
            return Err(HealthError::Full {
                what: "provider callers",
                bound: self.max_nodes,
            });
        }
        let c = window.nodes.entry(node).or_default();
        c.calls = c.calls.saturating_add(1);
        if outcome.is_err() {
            c.errors = c.errors.saturating_add(1);
        }
        Ok(())
    }

    /// Close every window that ended by `now_ns`.
    pub fn tick(&mut self, now_ns: u64, causes: &mut Vec<Cause>) -> Result<(), HealthError> {
        if now_ns < self.window_start {
            return Err(HealthError::TimeBackwards {
                what: "provider health",
                now_ns,
                last_ns: self.window_start,
            });
        }
        if now_ns < self.window_start.saturating_add(WINDOW_NS) {
            return Ok(());
        }
        self.close(causes);
        let skipped = now_ns.saturating_sub(self.window_start) / WINDOW_NS;
        self.window_start = skipped
            .checked_mul(WINDOW_NS)
            .and_then(|d| self.window_start.checked_add(d))
            .ok_or(HealthError::Overflow("provider window"))?;
        Ok(())
    }

    fn close(&mut self, causes: &mut Vec<Cause>) {
        for (provider, window) in &mut self.providers {
            let callers = window.nodes.len();
            let failing = window
                .nodes
                .values()
                .filter(|c| c.errors > 0 && c.errors.saturating_mul(2) >= c.calls)
                .count();
            let was = self.degraded.get(provider).copied().unwrap_or(false);
            let now = callers >= MIN_PEERS && failing.saturating_mul(2) > callers;
            if now != was {
                causes.push(if now {
                    Cause::Provider {
                        provider: *provider,
                    }
                } else {
                    Cause::ProviderRecovered {
                        provider: *provider,
                    }
                });
                self.degraded.insert(*provider, now);
            }
            // A node's own path is charged only when its errors are its own: fewer than MIN_PEERS
            // callers saw any error, and at least MIN_PEERS saw none. An outage's first window, when
            // only some of its callers have failed half their calls yet, charges nobody.
            let erring = window.nodes.values().filter(|c| c.errors > 0).count();
            let clean = callers.saturating_sub(erring);
            if !now && erring < MIN_PEERS && clean >= MIN_PEERS {
                for (node, c) in &window.nodes {
                    let evidence = c.calls >= MIN_OBSERVATIONS;
                    let failing_path = c.errors.saturating_mul(2) >= c.calls && c.errors > 0;
                    if evidence && failing_path {
                        causes.push(Cause::NodePath {
                            node: *node,
                            provider: *provider,
                        });
                    }
                }
            }
            window.nodes.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = WINDOW_NS;

    /// A4 stuck loop: the third identical call without progress is a loop; progress forgives.
    #[test]
    fn a_third_identical_call_without_progress_is_a_loop() {
        let mut d = LoopDetector::new(8);
        assert_eq!(d.call(1, 42).unwrap(), None);
        assert_eq!(d.call(1, 7).unwrap(), None);
        assert_eq!(d.call(1, 42).unwrap(), None);
        d.progress(1);
        assert_eq!(d.call(1, 42).unwrap(), None);
        assert_eq!(d.call(1, 42).unwrap(), None);
        assert_eq!(
            d.call(1, 42).unwrap(),
            Some(Stuck::Repeated {
                fingerprint: 42,
                times: 3
            })
        );
        // Another session's calls are its own.
        assert_eq!(d.call(2, 42).unwrap(), None);
    }

    /// A4: an A-B-A-B-A loop is caught as well as A-A-A, and many distinct calls with no progress hit
    /// the bound, which is a verdict.
    #[test]
    fn alternating_loops_and_aimless_sessions_are_caught() {
        let mut d = LoopDetector::new(8);
        let mut verdict = None;
        for f in [1, 2, 1, 2, 1] {
            verdict = d.call(1, f).unwrap();
        }
        assert_eq!(
            verdict,
            Some(Stuck::Repeated {
                fingerprint: 1,
                times: 3
            })
        );
        for f in 0..MAX_DISTINCT_CALLS as u64 {
            assert_eq!(d.call(2, f).unwrap(), None);
        }
        assert_eq!(
            d.call(2, u64::MAX).unwrap(),
            Some(Stuck::NoProgress {
                calls: MAX_DISTINCT_CALLS
            })
        );
        let mut full = LoopDetector::new(1);
        full.call(1, 1).unwrap();
        assert!(matches!(full.call(2, 1), Err(HealthError::Full { .. })));
    }

    /// A4 quota shown as slowness: rate limits and exhausted quota are charged to the account at the
    /// first error, never to the node or the provider.
    #[test]
    fn quota_is_the_accounts() {
        let mut p = ProviderHealth::new(8, 64, 0);
        let mut causes = Vec::new();
        p.call(
            1,
            5,
            9,
            10,
            Err(ProviderError::RateLimited {
                retry_after_ns: Some(30 * S),
            }),
            &mut causes,
        )
        .unwrap();
        p.call(1, 5, 9, 20, Err(ProviderError::QuotaExhausted), &mut causes)
            .unwrap();
        assert_eq!(
            causes,
            [
                Cause::Account {
                    account: 9,
                    until_ns: Some(10 + 30 * S)
                },
                Cause::Account {
                    account: 9,
                    until_ns: None
                },
            ]
        );
        for n in 0..10 {
            for k in 0..10 {
                p.call(
                    1,
                    n,
                    9,
                    S + k,
                    Err(ProviderError::QuotaExhausted),
                    &mut causes,
                )
                .unwrap();
            }
        }
        p.tick(3 * S, &mut causes).unwrap();
        assert!(causes.iter().all(|c| matches!(c, Cause::Account { .. })));
    }

    /// A4 partial provider outage: overload on most callers marks the provider, not a node; it
    /// recovers when calls succeed again.
    #[test]
    fn a_provider_outage_is_the_providers() {
        let mut p = ProviderHealth::new(8, 64, 0);
        let mut causes = Vec::new();
        for n in 0..10 {
            for k in 0..10 {
                let out = if n < 7 {
                    Err(ProviderError::Overloaded)
                } else {
                    Ok(())
                };
                p.call(1, n, n, k * (S / 10), out, &mut causes).unwrap();
            }
        }
        p.tick(S, &mut causes).unwrap();
        assert_eq!(causes, [Cause::Provider { provider: 1 }]);
        assert!(p.degraded(1));
        for n in 0..10 {
            p.call(1, n, n, S + 5, Ok(()), &mut causes).unwrap();
        }
        p.tick(2 * S, &mut causes).unwrap();
        assert_eq!(
            causes.last(),
            Some(&Cause::ProviderRecovered { provider: 1 })
        );
    }

    /// A node whose own path fails while its peers' calls succeed is charged to that node's path.
    #[test]
    fn one_nodes_failing_path_is_the_nodes() {
        let mut p = ProviderHealth::new(8, 64, 0);
        let mut causes = Vec::new();
        for n in 0..6 {
            for k in 0..10 {
                let out = if n == 3 {
                    Err(ProviderError::Timeout)
                } else {
                    Ok(())
                };
                p.call(2, n, n, k, out, &mut causes).unwrap();
            }
        }
        p.tick(S, &mut causes).unwrap();
        assert_eq!(
            causes,
            [Cause::NodePath {
                node: 3,
                provider: 2
            }]
        );
        assert!(!p.degraded(2));
    }

    /// Hostile input: time backwards and full maps are typed refusals.
    #[test]
    fn hostile_input_is_refused_typed() {
        let mut p = ProviderHealth::new(1, 1, 10);
        let mut c = Vec::new();
        assert!(matches!(
            p.call(1, 1, 1, 0, Ok(()), &mut c),
            Err(HealthError::TimeBackwards { .. })
        ));
        p.call(1, 1, 1, 10, Ok(()), &mut c).unwrap();
        assert!(matches!(
            p.call(2, 1, 1, 10, Ok(()), &mut c),
            Err(HealthError::Full { .. })
        ));
        assert!(matches!(
            p.call(1, 2, 1, 10, Ok(()), &mut c),
            Err(HealthError::Full { .. })
        ));
        p.tick(u64::MAX, &mut c).unwrap();
    }
}
