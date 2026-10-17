//! What the front door's DNS server says to one query: answered from its own names, forwarded
//! upstream (resolver mode), or refused. Pure: the sockets are in [`super::dns`].
//!
//! * The domain, each alias and every name under the domain (`*.kiln.home`, one wildcard) are the
//!   front door's: A and AAAA come from the configured addresses, else from the interface the query
//!   arrived on (the address facing the asker); ANY gets both; every other type gets an empty
//!   answer (NOERROR, no records) with an SOA in the authority section, so resolvers cache that it
//!   has none (RFC 2308). An SOA query for the domain or an alias gets the SOA.
//! * Anything else: forwarded in resolver mode, when the asker wants recursion and is on a
//!   private network (no open resolver for the internet); refused otherwise (REFUSED), and always
//!   in authoritative mode.
//! * Responses, other opcodes (NOTIMP) and more or fewer than one question (FORMERR) are not
//!   answered from the zone; a response is dropped, never answered.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::dns::{self, Message, Name, Question, RData, Record};

use super::DnsMode;

/// TTL of the front door's own answers: short, so a changed address is picked up soon.
pub const TTL: u32 = 60;
const RCODE_REFUSED: u16 = 5;
const RCODE_SERVFAIL: u16 = 2;
/// Header flag: recursion available.
const FLAG_RECURSION_AVAILABLE: u16 = 0x0080;
/// The UDP payload size kiln advertises in its EDNS(0) answers.
const EDNS_SIZE: u16 = 1232;

/// The names a front door answers for, and how.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Zone {
    /// The domain: itself and every name under it.
    pub domain: Option<Name>,
    /// Exact names besides.
    pub aliases: Vec<Name>,
    pub mode: DnsMode,
    /// Addresses to answer with; empty: the asker's interface's ([`Facing`]).
    pub addresses: Vec<IpAddr>,
}

/// This machine's addresses on the interface a query arrived on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facing {
    pub v4: Vec<Ipv4Addr>,
    pub v6: Vec<Ipv6Addr>,
}

/// What to do with a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Send this.
    Reply(Message),
    /// Forward the query upstream as it is (resolver mode).
    Forward,
    /// Not a query: say nothing.
    Drop,
}

impl Zone {
    pub fn new(settings: &super::Settings) -> Self {
        let domain = (!settings.domain.is_empty()).then(|| Name::parse(&settings.domain));
        Self {
            domain,
            aliases: settings.aliases.iter().map(|a| Name::parse(a)).collect(),
            mode: settings.dns.mode,
            addresses: settings.dns.addresses.iter().filter_map(|a| a.parse().ok()).collect(),
        }
    }

    /// The front door's name `name` falls under: its own (exact alias, the domain, or a name
    /// under the domain), as the apex its SOA is for.
    fn apex_of(&self, name: &Name) -> Option<Name> {
        if let Some(a) = self.aliases.iter().find(|a| *a == name) {
            return Some(a.clone());
        }
        self.domain.as_ref().filter(|d| name.is_under(d)).cloned()
    }

    /// What to say to `q`, which came from a host that may use recursion (`may_recurse`: on a
    /// private network) over an interface where this machine has `facing`.
    pub fn answer(&self, q: &Message, facing: &Facing, may_recurse: bool) -> Answer {
        if q.is_response() {
            return Answer::Drop;
        }
        let resolver = self.mode == DnsMode::Resolver;
        let ra = if resolver && may_recurse { FLAG_RECURSION_AVAILABLE } else { 0 };
        if q.opcode() != 0 {
            return Answer::Reply(reply(q, dns::RCODE_NOTIMP | ra, Vec::new(), Vec::new()));
        }
        let [question] = q.questions.as_slice() else {
            return Answer::Reply(reply(q, dns::RCODE_FORMERR | ra, Vec::new(), Vec::new()));
        };
        let class_ok = question.qclass == dns::CLASS_IN || question.qclass == dns::CLASS_ANY;
        if let Some(apex) = self.apex_of(&question.name).filter(|_| class_ok) {
            let (v4, v6) = self.addresses_for(facing);
            let name = &question.name;
            let a = || v4.iter().map(|ip| Record::new(name.clone(), TTL, false, RData::A(*ip)));
            let aaaa = || v6.iter().map(|ip| Record::new(name.clone(), TTL, false, RData::Aaaa(*ip)));
            let answers: Vec<Record> = match question.qtype {
                dns::TYPE_A => a().collect(),
                dns::TYPE_AAAA => aaaa().collect(),
                dns::TYPE_ANY => a().chain(aaaa()).collect(),
                dns::TYPE_SOA if *name == apex => vec![soa(&apex)],
                _ => Vec::new(),
            };
            // No records of that type: say so with the SOA, for negative caching.
            let authorities = if answers.is_empty() { vec![soa(&apex)] } else { Vec::new() };
            return Answer::Reply(reply(q, dns::RCODE_NOERROR | dns::FLAG_AUTHORITATIVE | ra, answers, authorities));
        }
        if resolver && may_recurse && q.flags & dns::FLAG_RECURSION_DESIRED != 0 {
            return Answer::Forward;
        }
        Answer::Reply(reply(q, RCODE_REFUSED | ra, Vec::new(), Vec::new()))
    }

    fn addresses_for(&self, facing: &Facing) -> (Vec<Ipv4Addr>, Vec<Ipv6Addr>) {
        if self.addresses.is_empty() {
            return (facing.v4.clone(), facing.v6.clone());
        }
        let v4 = self.addresses.iter().filter_map(|a| if let IpAddr::V4(v) = a { Some(*v) } else { None }).collect();
        let v6 = self.addresses.iter().filter_map(|a| if let IpAddr::V6(v) = a { Some(*v) } else { None }).collect();
        (v4, v6)
    }
}

/// A response to `q`: its id and question, recursion desired as it asked, and `flags` (the
/// response code and AA/RA). EDNS(0) is answered with EDNS(0).
fn reply(q: &Message, flags: u16, answers: Vec<Record>, authorities: Vec<Record>) -> Message {
    let edns = q.additionals.iter().any(|r| r.rtype() == dns::TYPE_OPT);
    let additionals = if edns {
        vec![Record { name: Name::default(), class: EDNS_SIZE, cache_flush: false, ttl: 0, data: RData::Other { rtype: dns::TYPE_OPT, data: Vec::new() } }]
    } else {
        Vec::new()
    };
    Message {
        id: q.id,
        flags: dns::FLAG_RESPONSE | (q.flags & dns::FLAG_RECURSION_DESIRED) | flags,
        questions: q.questions.iter().take(1).cloned().collect(),
        answers,
        authorities,
        additionals,
    }
}

/// A server failure for `q` (no upstream answered).
pub fn servfail(q: &Message) -> Message {
    reply(q, RCODE_SERVFAIL | FLAG_RECURSION_AVAILABLE, Vec::new(), Vec::new())
}

/// The zone's SOA (RFC 1035 §3.3.13): `ns.<apex>`, `hostmaster.<apex>`, a fixed serial, and the
/// TTL as the negative-caching minimum.
fn soa(apex: &Name) -> Record {
    let mut data = Vec::new();
    for n in [Name::child("ns", apex), Name::child("hostmaster", apex)] {
        data.extend(n.canonical_wire().unwrap_or_else(|_| vec![0]));
    }
    for v in [1u32, 3600, 600, 86_400, TTL] {
        data.extend_from_slice(&v.to_be_bytes());
    }
    Record::new(apex.clone(), TTL, false, RData::Other { rtype: dns::TYPE_SOA, data })
}

/// A query for tests and probes.
pub fn query(name: &str, qtype: u16, recursion: bool) -> Message {
    Message {
        id: 0x4b49,
        flags: if recursion { dns::FLAG_RECURSION_DESIRED } else { 0 },
        questions: vec![Question { name: Name::parse(name), qtype, qclass: dns::CLASS_IN, unicast: false }],
        ..Message::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontdoor::{Dns, Settings};

    fn zone(mode: DnsMode) -> Zone {
        Zone::new(&Settings {
            domain: "kiln.home".into(),
            aliases: vec!["ci.home".into()],
            dns: Dns { enabled: true, mode, ..Dns::default() },
            ..Settings::default()
        })
    }

    fn lan() -> Facing {
        Facing { v4: vec![Ipv4Addr::new(192, 168, 1, 77)], v6: vec!["fd00::77".parse().unwrap_or(Ipv6Addr::LOCALHOST)] }
    }

    fn reply_of(a: Answer) -> Message {
        match a {
            Answer::Reply(m) => m,
            other => panic_free_empty(other),
        }
    }

    /// Tests may not panic: an unexpected verdict becomes a message no assertion accepts.
    fn panic_free_empty(_: Answer) -> Message {
        Message { id: 0xdead, flags: 0x000f, ..Message::default() }
    }

    fn records(m: &Message) -> Vec<RData> {
        m.answers.iter().map(|r| r.data.clone()).collect()
    }

    #[test]
    fn the_domain_its_aliases_and_names_under_it_answer_a_and_aaaa() -> Result<(), crate::dns::Error> {
        let z = zone(DnsMode::Resolver);
        for name in ["kiln.home", "KILN.Home.", "ci.home", "anything.kiln.home", "a.b.kiln.home"] {
            let m = reply_of(z.answer(&query(name, dns::TYPE_A, true), &lan(), true));
            assert_eq!(m.rcode(), dns::RCODE_NOERROR, "{name}");
            assert_ne!(m.flags & dns::FLAG_AUTHORITATIVE, 0, "{name}: authoritative");
            assert_eq!(records(&m), [RData::A(Ipv4Addr::new(192, 168, 1, 77))], "{name}");
            assert_eq!(m.answers.first().map(|r| (r.name.clone(), r.ttl)), Some((Name::parse(name), TTL)), "{name}: the name asked");
            assert_eq!(m.id, 0x4b49);
            assert_eq!(m.questions.len(), 1);
            // It goes through the wire format and back.
            assert_eq!(Message::decode(&m.encode()?)?, m);
        }
        let m = reply_of(z.answer(&query("kiln.home", dns::TYPE_AAAA, true), &lan(), true));
        assert_eq!(records(&m), [RData::Aaaa("fd00::77".parse().unwrap_or(Ipv6Addr::UNSPECIFIED))]);
        let m = reply_of(z.answer(&query("kiln.home", dns::TYPE_ANY, true), &lan(), true));
        assert_eq!(m.answers.len(), 2);
        // Configured addresses win over the interface's.
        let mut fixed = z.clone();
        fixed.addresses = vec!["10.0.0.9".parse().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))];
        let m = reply_of(fixed.answer(&query("kiln.home", dns::TYPE_A, true), &lan(), true));
        assert_eq!(records(&m), [RData::A(Ipv4Addr::new(10, 0, 0, 9))]);
        let m = reply_of(fixed.answer(&query("kiln.home", dns::TYPE_AAAA, true), &lan(), true));
        assert!(m.answers.is_empty() && m.authorities.len() == 1, "no IPv6 configured: no AAAA, and an SOA");
        Ok(())
    }

    #[test]
    fn other_types_get_an_empty_answer_with_the_soa() -> Result<(), crate::dns::Error> {
        let z = zone(DnsMode::Authoritative);
        // Browsers ask for HTTPS records (type 65): an answer at once, not a timeout.
        for qtype in [65, dns::TYPE_TXT, 15] {
            let m = reply_of(z.answer(&query("kiln.home", qtype, true), &lan(), true));
            assert_eq!(m.rcode(), dns::RCODE_NOERROR);
            assert!(m.answers.is_empty());
            assert_eq!(m.authorities.first().map(|r| (r.rtype(), r.name.clone())), Some((dns::TYPE_SOA, Name::parse("kiln.home"))));
            Message::decode(&m.encode()?)?;
        }
        let m = reply_of(z.answer(&query("ci.home", dns::TYPE_SOA, true), &lan(), true));
        assert_eq!(m.answers.first().map(Record::rtype), Some(dns::TYPE_SOA));
        // A name under the domain has no SOA of its own: the domain's goes in the authority section.
        let m = reply_of(z.answer(&query("x.kiln.home", dns::TYPE_SOA, true), &lan(), true));
        assert!(m.answers.is_empty() && m.authorities.first().is_some_and(|r| r.name == Name::parse("kiln.home")));
        Ok(())
    }

    #[test]
    fn a_resolver_forwards_the_rest_and_an_authoritative_server_refuses_it() {
        let resolver = zone(DnsMode::Resolver);
        let authoritative = zone(DnsMode::Authoritative);
        for name in ["example.com", "x.ci.home", "home", "kiln.lan"] {
            assert_eq!(resolver.answer(&query(name, dns::TYPE_A, true), &lan(), true), Answer::Forward, "{name}");
            let m = reply_of(authoritative.answer(&query(name, dns::TYPE_A, true), &lan(), true));
            assert_eq!(m.rcode(), RCODE_REFUSED, "{name}");
            assert!(m.answers.is_empty());
            assert_eq!(m.flags & FLAG_RECURSION_AVAILABLE, 0, "an authoritative server offers no recursion");
        }
        // Only for askers on private networks, and only when they want recursion.
        let m = reply_of(resolver.answer(&query("example.com", dns::TYPE_A, true), &lan(), false));
        assert_eq!(m.rcode(), RCODE_REFUSED);
        let m = reply_of(resolver.answer(&query("example.com", dns::TYPE_A, false), &lan(), true));
        assert_eq!(m.rcode(), RCODE_REFUSED);
        // Its own names are answered to anyone.
        let m = reply_of(resolver.answer(&query("kiln.home", dns::TYPE_A, true), &lan(), false));
        assert_eq!(records(&m).len(), 1);
    }

    #[test]
    fn odd_messages_are_dropped_or_refused_politely() {
        let z = zone(DnsMode::Resolver);
        let mut response = query("kiln.home", dns::TYPE_A, true);
        response.flags |= dns::FLAG_RESPONSE;
        assert_eq!(z.answer(&response, &lan(), true), Answer::Drop);
        let mut update = query("kiln.home", dns::TYPE_A, true);
        update.flags |= dns::OPCODE_UPDATE;
        assert_eq!(reply_of(z.answer(&update, &lan(), true)).rcode(), dns::RCODE_NOTIMP);
        let mut two = query("kiln.home", dns::TYPE_A, true);
        two.questions.extend(query("ci.home", dns::TYPE_A, true).questions);
        assert_eq!(reply_of(z.answer(&two, &lan(), true)).rcode(), dns::RCODE_FORMERR);
        let none = Message { flags: dns::FLAG_RECURSION_DESIRED, ..Message::default() };
        assert_eq!(reply_of(z.answer(&none, &lan(), true)).rcode(), dns::RCODE_FORMERR);
        // CHAOS-class questions about the domain are not the zone's.
        let mut chaos = query("kiln.home", dns::TYPE_TXT, true);
        if let Some(q) = chaos.questions.first_mut() {
            q.qclass = 3;
        }
        assert_eq!(z.answer(&chaos, &lan(), true), Answer::Forward);
        // EDNS(0) is answered with EDNS(0).
        let edns = Message::query(7, Name::parse("kiln.home"), dns::TYPE_A, true);
        let m = reply_of(z.answer(&edns, &lan(), true));
        assert_eq!(m.additionals.first().map(|r| (r.rtype(), r.class)), Some((dns::TYPE_OPT, EDNS_SIZE)));
        assert_eq!(servfail(&edns).rcode(), RCODE_SERVFAIL);
    }

    #[test]
    fn no_domain_answers_nothing_of_its_own() {
        let z = Zone::new(&Settings::default());
        assert_eq!(z.answer(&query("kiln.home", dns::TYPE_A, true), &lan(), true), Answer::Forward);
    }
}
