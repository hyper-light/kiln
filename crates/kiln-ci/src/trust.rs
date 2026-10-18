//! Untrusted code (docs/ci.md W18-R05..R08), the pure half: which untrusted runs wait for a
//! maintainer, and where a leg's network policy lets it go.

use serde::{Deserialize, Serialize};

use crate::wf::ir::YVal;

/// `ci.untrusted.require_approval` (W18-R06), GitHub's options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    /// Runs of contributors with no commit merged yet.
    #[default]
    FirstTime,
    /// Runs of everyone without write access.
    AllExternal,
    None,
}

/// GitHub's `author_association`s of those with write access.
const INSIDE: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Whether a run waits for a maintainer's approval before any of its jobs start. Only untrusted
/// runs ever do: under `first-time`, those whose author has had nothing merged
/// (`author_association` `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER`, `NONE`, `MANNEQUIN`, or not
/// known); under `all-external`, any whose author has no write access.
pub fn needs_approval(policy: Approval, trusted: bool, association: Option<&str>) -> bool {
    let is = |list: &[&str]| association.is_some_and(|a| list.iter().any(|x| x.eq_ignore_ascii_case(a)));
    match policy {
        _ if trusted => false,
        Approval::None => false,
        Approval::AllExternal => !is(INSIDE),
        Approval::FirstTime => !is(INSIDE) && !is(&["CONTRIBUTOR"]),
    }
}

/// Splits `host[:port]` (`[v6]:port` for IPv6) into its host, lower-cased, and port.
pub fn split_host(s: &str) -> (String, Option<u16>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').unwrap_or((rest, ""));
        return (h.to_ascii_lowercase(), tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match s.rsplit_once(':') {
        // One colon: a port; more: a bare IPv6 address.
        Some((h, p)) if !h.contains(':') => (h.to_ascii_lowercase(), p.parse().ok()),
        _ => (s.to_ascii_lowercase(), None),
    }
}

/// Whether `pattern` lets a leg reach `host` on `port`: a name (`example.com`), any name under
/// one (`*.example.com`, not itself), anything (`*`), an address as written; each with an
/// optional port (`example.com:443`), else any port. Names are compared without case and without
/// a trailing dot.
pub fn host_matches(pattern: &str, host: &str, port: u16) -> bool {
    let (p, p_port) = split_host(pattern);
    if p_port.is_some_and(|pp| pp != port) {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let p = p.trim_end_matches('.');
    match p.strip_prefix("*.") {
        _ if p == "*" => true,
        Some(domain) => host.len() > domain.len() + 1 && host.ends_with(domain) && host.as_bytes().get(host.len() - domain.len() - 1) == Some(&b'.'),
        None => host == p,
    }
}

/// Whether any of `allow` lets a leg reach `host:port`.
pub fn allowed(allow: &[String], host: &str, port: u16) -> bool {
    allow.iter().any(|p| host_matches(p, host, port))
}

/// Where a leg may connect (W18-R08), from its job's `network: { egress: none | open | { allow:
/// [hosts] } }` (kiln) and its trust: `None` when it is open (no proxy), else the hosts it may
/// reach besides kiln's own server, which every leg reaches. An untrusted leg is never open: it
/// gets the server's `ci.untrusted.allow_hosts`, and its own list narrows them.
pub fn egress(network: Option<&YVal>, trusted: bool, untrusted_allow: &[String]) -> Result<Option<Vec<String>>, String> {
    let own = match network.map(|n| n.get("egress").unwrap_or(n)) {
        None | Some(YVal::Null) => None,
        Some(v) if v.as_str() == Some("open") => None,
        Some(v) if v.as_str() == Some("none") => Some(Vec::new()),
        Some(v) => match v.get("allow") {
            Some(YVal::Seq(list)) => Some(list.iter().map(|h| h.as_str().map(str::to_owned).ok_or_else(|| "`network.egress.allow` lists hosts".to_owned())).collect::<Result<Vec<_>, _>>()?),
            Some(YVal::Str(t)) => Some(t.src.split([',', '\n']).map(str::trim).filter(|h| !h.is_empty()).map(str::to_owned).collect()),
            _ => return Err("`network.egress` is `none`, `open` or `{ allow: [hosts] }`".into()),
        },
    };
    if trusted {
        return Ok(own);
    }
    Ok(Some(match own {
        None => untrusted_allow.to_vec(),
        // Only what both allow: a pattern of the leg's that the server's cover, host by host.
        Some(list) => list.into_iter().filter(|h| covered(untrusted_allow, h)).collect(),
    }))
}

/// Whether every host `pattern` names is one `allow` lets through.
fn covered(allow: &[String], pattern: &str) -> bool {
    let (host, port) = split_host(pattern);
    if host.starts_with('*') {
        // A wildcard of the leg's: only under one of the server's own as wide, or `*`.
        return allow.iter().any(|a| {
            let (ah, ap) = split_host(a);
            (ap.is_none() || ap == port) && (ah == "*" || ah == host || host.strip_prefix("*.").is_some_and(|d| ah.strip_prefix("*.").is_some_and(|ad| d == ad || d.ends_with(&format!(".{ad}")))))
        });
    }
    match port {
        Some(p) => allowed(allow, &host, p),
        None => allow.iter().any(|a| {
            let (ah, ap) = split_host(a);
            ap.is_none() && host_matches(&ah, &host, 0)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::ir::Tpl;

    #[test]
    fn w18_r06_approval() {
        let (first, external) = (Approval::FirstTime, Approval::AllExternal);
        assert!(!needs_approval(first, true, None), "trusted runs never wait");
        assert!(needs_approval(first, false, Some("FIRST_TIME_CONTRIBUTOR")) && needs_approval(first, false, Some("first_timer")) && needs_approval(first, false, None));
        assert!(!needs_approval(first, false, Some("CONTRIBUTOR")) && !needs_approval(first, false, Some("MEMBER")));
        assert!(needs_approval(external, false, Some("CONTRIBUTOR")) && !needs_approval(external, false, Some("COLLABORATOR")));
        assert!(!needs_approval(Approval::None, false, Some("FIRST_TIMER")));
        assert_eq!(serde_json::from_str::<Approval>("\"all-external\"").ok(), Some(external));
    }

    #[test]
    fn w18_r08_hosts() {
        assert!(host_matches("registry.npmjs.org", "Registry.NPMJS.org.", 443));
        assert!(host_matches("*.pypi.org", "files.pypi.org", 443) && !host_matches("*.pypi.org", "pypi.org", 443) && !host_matches("*.pypi.org", "evilpypi.org", 443));
        assert!(host_matches("github.com:443", "github.com", 443) && !host_matches("github.com:443", "github.com", 22));
        assert!(host_matches("*", "anything.example", 1) && host_matches("10.0.0.5", "10.0.0.5", 80));
        assert_eq!(split_host("[::1]:8080"), ("::1".into(), Some(8080)));
        assert_eq!(split_host("::1"), ("::1".into(), None));
        assert!(allowed(&["a.com".into(), "*.b.com".into()], "x.b.com", 80) && !allowed(&[], "a.com", 80));
    }

    #[test]
    fn w18_r08_egress() -> Result<(), String> {
        let t = |s: &str| YVal::Str(Tpl::new(s, Default::default()));
        let allow = |hosts: &[&str]| YVal::Map(vec![("egress".into(), YVal::Map(vec![("allow".into(), YVal::Seq(hosts.iter().map(|h| t(h)).collect()))]))]);
        let server = vec!["registry.npmjs.org".to_owned(), "*.pypi.org".to_owned()];
        assert_eq!(egress(None, true, &server)?, None, "trusted and silent: open");
        assert_eq!(egress(Some(&YVal::Map(vec![("egress".into(), t("none"))])), true, &server)?, Some(vec![]));
        assert_eq!(egress(Some(&allow(&["github.com"])), true, &server)?, Some(vec!["github.com".to_owned()]));
        // Untrusted: never open, and its own list only narrows the server's.
        assert_eq!(egress(Some(&YVal::Map(vec![("egress".into(), t("open"))])), false, &server)?, Some(server.clone()));
        assert_eq!(egress(Some(&allow(&["registry.npmjs.org", "files.pypi.org", "*.pypi.org", "evil.com", "*.org"])), false, &server)?, Some(vec!["registry.npmjs.org".to_owned(), "files.pypi.org".to_owned(), "*.pypi.org".to_owned()]));
        assert!(egress(Some(&YVal::Map(vec![("egress".into(), YVal::Num(1.0))])), true, &server).is_err());
        Ok(())
    }
}
