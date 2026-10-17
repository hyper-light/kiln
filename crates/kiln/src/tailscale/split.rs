//! Keeping a tailnet's split DNS pointed at kiln's front doors: what to change, from what kiln
//! wants, what it set before and what the tailnet says now. Pure; the calls are in [`super::api`].
//!
//! * A domain kiln wants goes to exactly the addresses wanted, whatever pointed it elsewhere
//!   before (the person chose this runner as the domain's DNS); what it replaced is reported.
//! * A domain kiln set and wants no more is cleared, unless someone changed it since: then it is
//!   left as it is, and is no longer kiln's.
//! * A domain still settling (a runner that serves it has not said where it listens) is left
//!   alone either way, so a server restart or a runner away never takes a working route down.

use std::collections::{BTreeMap, BTreeSet};

use super::{Resolver, normalize};

/// The tailnet's split DNS: domain (normalized) → resolvers.
pub type Routes = BTreeMap<String, Vec<Resolver>>;

/// What to change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Point these domains at these addresses (replacing what is there).
    pub set: BTreeMap<String, Vec<String>>,
    /// Domains kiln pointed somewhere and serves no more: their routes go.
    pub clear: Vec<String>,
    /// Domains kiln set that someone changed since: left as they are, and no longer kiln's.
    pub disown: Vec<String>,
    /// What `set` replaces, where that was not kiln's own route.
    pub replaced: BTreeMap<String, Vec<String>>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.clear.is_empty()
    }
}

fn addresses(rs: &[Resolver]) -> BTreeSet<String> {
    rs.iter().map(|r| r.addr.trim().to_ascii_lowercase()).collect()
}

fn same(rs: &[Resolver], want: &[String]) -> bool {
    addresses(rs) == want.iter().map(|a| a.trim().to_ascii_lowercase()).collect()
}

/// The changes that make `current` say `want` (domain → addresses), given what kiln set before
/// (`owned`) and the domains still settling (`pending`). `exit_nodes`: routes can carry "use
/// with exit node", and every route of kiln's should.
pub fn plan(want: &BTreeMap<String, Vec<String>>, pending: &BTreeSet<String>, owned: &BTreeMap<String, Vec<String>>, current: &Routes, exit_nodes: bool) -> Plan {
    let mut p = Plan::default();
    for (domain, addrs) in want {
        if pending.contains(domain) || addrs.is_empty() {
            continue;
        }
        let have = current.get(domain);
        let fine = have.is_some_and(|h| same(h, addrs) && (!exit_nodes || h.iter().all(|r| r.exit_node)));
        if fine {
            continue;
        }
        p.set.insert(domain.clone(), addrs.clone());
        if let Some(h) = have.filter(|h| !h.is_empty())
            && owned.get(domain).is_none_or(|o| !same(h, o))
        {
            p.replaced.insert(domain.clone(), h.iter().map(|r| r.addr.clone()).collect());
        }
    }
    for (domain, was) in owned {
        if want.contains_key(domain) || pending.contains(domain) {
            continue;
        }
        match current.get(domain) {
            None => {}
            Some(h) if same(h, was) => p.clear.push(domain.clone()),
            Some(_) => p.disown.push(domain.clone()),
        }
    }
    p
}

/// What kiln owns once `plan` is applied: every domain it wants and is not waiting on, plus the
/// settling ones it had.
pub fn owned_after(want: &BTreeMap<String, Vec<String>>, pending: &BTreeSet<String>, owned: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = want.iter().filter(|(d, a)| !pending.contains(*d) && !a.is_empty()).map(|(d, a)| (d.clone(), a.clone())).collect();
    for (d, a) in owned {
        if pending.contains(d) {
            out.insert(d.clone(), a.clone());
        }
    }
    out
}

/// The body of `PATCH /dns/split-dns` for `plan`: each domain set to its addresses or cleared.
pub fn patch_of(plan: &Plan) -> BTreeMap<String, Option<Vec<String>>> {
    let mut out: BTreeMap<String, Option<Vec<String>>> = plan.set.iter().map(|(d, a)| (d.clone(), Some(a.clone()))).collect();
    for d in &plan.clear {
        out.insert(d.clone(), None);
    }
    out
}

/// The split DNS in a DNS configuration (`GET /dns/configuration`).
pub fn routes_of_configuration(config: &serde_json::Value) -> Routes {
    let mut out = Routes::new();
    if let Some(m) = config.get("splitDNS").and_then(serde_json::Value::as_object) {
        for (domain, list) in m {
            let Some(list) = list.as_array() else { continue };
            let rs = list
                .iter()
                .filter_map(|r| {
                    let addr = r.get("address")?.as_str()?.to_owned();
                    Some(Resolver { addr, exit_node: r.get("useWithExitNode").and_then(serde_json::Value::as_bool).unwrap_or(false) })
                })
                .collect();
            out.insert(normalize(domain), rs);
        }
    }
    out
}

/// The split DNS from `GET /dns/split-dns` (no exit-node flags there).
pub fn routes_of_split_dns(m: &BTreeMap<String, Vec<String>>) -> Routes {
    m.iter().map(|(d, a)| (normalize(d), a.iter().map(|addr| Resolver { addr: addr.clone(), exit_node: false }).collect())).collect()
}

/// `config` with `plan` applied to its split DNS, everything else as it was: kiln's routes are
/// used with exit nodes too, and a domain is found whatever its case or final dot there.
pub fn apply_to_configuration(config: &serde_json::Value, plan: &Plan) -> serde_json::Value {
    let mut config = config.clone();
    let Some(obj) = config.as_object_mut() else { return config };
    let split = obj.entry("splitDNS").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !split.is_object() {
        *split = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(m) = split.as_object_mut() {
        let key_of = |m: &serde_json::Map<String, serde_json::Value>, d: &str| m.keys().find(|k| normalize(k) == d).cloned();
        for d in &plan.clear {
            if let Some(k) = key_of(m, d) {
                m.remove(&k);
            }
        }
        for (d, addrs) in &plan.set {
            let key = key_of(m, d).unwrap_or_else(|| d.clone());
            let list = addrs.iter().map(|a| serde_json::json!({ "address": a, "useWithExitNode": true })).collect();
            m.insert(key, serde_json::Value::Array(list));
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(addr: &str, exit_node: bool) -> Resolver {
        Resolver { addr: addr.into(), exit_node }
    }

    fn map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs.iter().map(|(d, a)| ((*d).to_owned(), a.iter().map(|x| (*x).to_owned()).collect())).collect()
    }

    #[test]
    fn wanted_domains_are_pointed_at_their_runner() {
        let want = map(&[("kiln.dev", &["100.64.0.5"]), ("ci.home", &["100.64.0.5"])]);
        let none = BTreeSet::new();
        // Nothing there yet: both set.
        let p = plan(&want, &none, &BTreeMap::new(), &Routes::new(), true);
        assert_eq!(p.set, want);
        assert!(p.clear.is_empty() && p.replaced.is_empty());
        assert_eq!(patch_of(&p).get("kiln.dev"), Some(&Some(vec!["100.64.0.5".to_owned()])));
        // Already right (with the exit-node flag): nothing to do.
        let current: Routes = [("kiln.dev".to_owned(), vec![r("100.64.0.5", true)]), ("ci.home".to_owned(), vec![r("100.64.0.5", true)])].into();
        assert!(plan(&want, &none, &want, &current, true).is_empty());
        // Right addresses but no exit-node flag: set again where flags can be set, not where they can't.
        let unflagged: Routes = [("kiln.dev".to_owned(), vec![r("100.64.0.5", false)]), ("ci.home".to_owned(), vec![r("100.64.0.5", false)])].into();
        assert_eq!(plan(&want, &none, &want, &unflagged, true).set.len(), 2);
        assert!(plan(&want, &none, &want, &unflagged, false).is_empty());
        // Somebody else's nameserver for the domain is replaced, and said so.
        let theirs: Routes = [("kiln.dev".to_owned(), vec![r("10.0.0.53", false)])].into();
        let p = plan(&want, &none, &BTreeMap::new(), &theirs, false);
        assert_eq!(p.replaced.get("kiln.dev"), Some(&vec!["10.0.0.53".to_owned()]));
        // A runner that moved (kiln's own old address) is updated without a "replaced".
        let moved: Routes = [("kiln.dev".to_owned(), vec![r("100.64.0.9", true)])].into();
        let p = plan(&want, &none, &map(&[("kiln.dev", &["100.64.0.9"])]), &moved, true);
        assert_eq!(p.set.get("kiln.dev"), Some(&vec!["100.64.0.5".to_owned()]));
        assert!(p.replaced.is_empty());
    }

    #[test]
    fn domains_given_up_are_cleared_unless_changed_by_someone() {
        let owned = map(&[("old.home", &["100.64.0.5"]), ("edited.home", &["100.64.0.5"]), ("gone.home", &["100.64.0.5"])]);
        let current: Routes = [
            ("old.home".to_owned(), vec![r("100.64.0.5", true)]),
            ("edited.home".to_owned(), vec![r("192.168.1.1", false)]),
            ("other.home".to_owned(), vec![r("1.1.1.1", false)]),
        ]
        .into();
        let p = plan(&BTreeMap::new(), &BTreeSet::new(), &owned, &current, true);
        assert_eq!(p.clear, ["old.home"]);
        assert_eq!(p.disown, ["edited.home"]);
        assert!(p.set.is_empty());
        assert_eq!(patch_of(&p).get("old.home"), Some(&None));
        assert!(!patch_of(&p).contains_key("other.home"), "never someone else's route");
        assert!(owned_after(&BTreeMap::new(), &BTreeSet::new(), &owned).is_empty());
    }

    #[test]
    fn settling_domains_are_left_alone() {
        let owned = map(&[("kiln.dev", &["100.64.0.5"])]);
        let current: Routes = [("kiln.dev".to_owned(), vec![r("100.64.0.5", true)])].into();
        let pending: BTreeSet<String> = ["kiln.dev".to_owned()].into();
        // After a server restart nobody has reported yet: the route stays, and stays kiln's.
        let p = plan(&BTreeMap::new(), &pending, &owned, &current, true);
        assert!(p.is_empty() && p.disown.is_empty());
        assert_eq!(owned_after(&BTreeMap::new(), &pending, &owned), owned);
        // Wanted elsewhere too, but still settling: not touched.
        let want = map(&[("kiln.dev", &["100.64.0.7"])]);
        assert!(plan(&want, &pending, &owned, &current, true).is_empty());
    }

    #[test]
    fn configurations_are_read_and_changed_in_place() -> anyhow::Result<()> {
        let config = serde_json::json!({
            "nameservers": [{ "address": "8.8.8.8", "useWithExitNode": false }],
            "splitDNS": {
                "Kiln.Dev.": [{ "address": "10.0.0.53" }],
                "old.home": [{ "address": "100.64.0.5", "useWithExitNode": true }],
                "corp.example": [{ "address": "10.1.1.1" }]
            },
            "searchPaths": ["corp.example"],
            "preferences": { "overrideLocalDNS": false, "magicDNS": true },
            "somethingNew": { "kept": true }
        });
        let routes = routes_of_configuration(&config);
        assert_eq!(routes.get("kiln.dev"), Some(&vec![r("10.0.0.53", false)]));
        assert_eq!(routes.get("old.home"), Some(&vec![r("100.64.0.5", true)]));
        let p = Plan { set: map(&[("kiln.dev", &["100.64.0.7"])]), clear: vec!["old.home".into()], ..Plan::default() };
        let out = apply_to_configuration(&config, &p);
        assert_eq!(out.get("splitDNS").and_then(|s| s.get("Kiln.Dev.")), Some(&serde_json::json!([{ "address": "100.64.0.7", "useWithExitNode": true }])));
        assert!(out.get("splitDNS").and_then(|s| s.get("old.home")).is_none());
        // Everything else exactly as it was, including fields kiln does not know.
        for key in ["nameservers", "searchPaths", "preferences", "somethingNew"] {
            assert_eq!(out.get(key), config.get(key), "{key}");
        }
        assert_eq!(out.get("splitDNS").and_then(|s| s.get("corp.example")), config.get("splitDNS").and_then(|s| s.get("corp.example")));
        // A configuration without split DNS gets one.
        let bare = apply_to_configuration(&serde_json::json!({ "preferences": { "magicDNS": true } }), &p);
        assert_eq!(routes_of_configuration(&bare).get("kiln.dev").map(Vec::len), Some(1));
        assert_eq!(routes_of_split_dns(&map(&[("A.b.", &["1.2.3.4"])])).get("a.b"), Some(&vec![r("1.2.3.4", false)]));
        Ok(())
    }
}
