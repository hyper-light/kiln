//! The custody model (docs/ci.md W27-R08 and W27-R10's base; the Transfers plan's step 15):
//! custodians hand things on to each other, directly and through relays, under kiln-sim's seeded
//! nemesis (what each sends kiln comes late, twice, out of order; a relay's own records can be lost
//! on their way), while relays misbehave at random: they drop what they carry (silently, or after
//! signing their pass of it), swap two things' bytes, change a byte (signing for the bytes they got
//! or for the ones they changed them to), replay a thing handed on before, and lose their own
//! records. Every record is signed under a test CA and checked by kiln-ci's custody core as kiln's
//! server checks it: `hop`, `walk`, `statement::gaps`. A taker checks what came against its giver's
//! hand-on before it uses it, as each of steps 11 to 14 refuses a thing whose digest isn't what was
//! handed on, and signs its receipt of what came either way. It checks:
//!
//! - safety: no chain with a break walks as unbroken (a thing whose taker got other bytes than its
//!   giver handed on never walks back unbroken from its receipt); nothing used differs from what its
//!   giver handed on; and
//!   nothing a relay passed on is unseen (a record of it lost on its way is a gap in the relay's
//!   numbers, once a later one comes);
//! - liveness: once the nemesis has stopped and the relays behave, every thing handed on reaches
//!   its taker, is used, and walks unbroken from its receipt to its giver's hand-on.
//!
//! Its power is shown by bugs planted on purpose ([`Plant`]): a relay that doesn't number its
//! records, a hop that doesn't check the digest, a walk that skips a hop. Each is found within the
//! budget of seeds, by a seed that passes without it.
//!
//! Left out: who may take custody (delegations, `custody::entitled`), kiln's takes for a custodian
//! that can't sign, and the paths' own transports (each tested end to end, `tests/transfers.rs`).

#[cfg(test)]
mod model {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use kiln_ci::identity::custody::{self, Body, Break, Kind};
    use kiln_ci::identity::statement::{self, Checked, PAYLOAD_CUSTODY, Statement, Subject};
    use kiln_sim::{Queue, Rng, Trace};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SigningKey as _, date_time_ymd};
    use serde_json::Value;
    use sha2::{Digest as _, Sha256};

    const SERVER: &str = "kiln://hyper-light/server";
    /// Inside every test certificate's validity: a run's clock starts here.
    const AT: i64 = 1_790_000_000;
    /// Events the model's queue holds, at the most.
    const QUEUE_MAX: usize = 1 << 16;
    /// The stream a relay numbers its passes on.
    const STREAM: &str = "relayed";

    /// Bugs planted on purpose: the model finds each within its budget of seeds.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Plant {
        /// Relays sign their passes with no number: one lost on its way leaves no gap.
        pub unnumbered: bool,
        /// kiln's hop doesn't check the digest: a thing changed on its way reads as the same.
        pub no_digest_check: bool,
        /// kiln's walk checks the receipt's own hop and goes no further back.
        pub skips_hops: bool,
    }

    /// What one run did, for the budget's check that the model tested what it says.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Counts {
        pub transfers: u64,
        pub relayed: u64,
        pub used: u64,
        pub refused: u64,
        pub changed: u64,
        pub swapped: u64,
        pub replayed: u64,
        pub dropped: u64,
        pub records_lost: u64,
        pub breaks_found: u64,
        pub gaps_found: u64,
        pub unbroken: u64,
    }

    impl Counts {
        pub fn add(&mut self, o: &Counts) {
            self.transfers += o.transfers;
            self.relayed += o.relayed;
            self.used += o.used;
            self.refused += o.refused;
            self.changed += o.changed;
            self.swapped += o.swapped;
            self.replayed += o.replayed;
            self.dropped += o.dropped;
            self.records_lost += o.records_lost;
            self.breaks_found += o.breaks_found;
            self.gaps_found += o.gaps_found;
            self.unbroken += o.unbroken;
        }
    }

    /// The test CA.
    struct Ca {
        der: Vec<u8>,
        issuer: Issuer<'static, KeyPair>,
    }

    impl Ca {
        fn new() -> anyhow::Result<Self> {
            let key = KeyPair::generate()?;
            let mut p = CertificateParams::new(Vec::<String>::new())?;
            p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            p.distinguished_name.push(rcgen::DnType::CommonName, "kiln CA");
            p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
            p.not_before = date_time_ymd(2025, 1, 1);
            p.not_after = date_time_ymd(2036, 1, 1);
            let cert = p.self_signed(&key)?;
            Ok(Self { der: cert.der().to_vec(), issuer: Issuer::new(p, key) })
        }
    }

    /// A custodian's key and certificate: what signs as it.
    struct Signer {
        uri: String,
        der: Vec<u8>,
        key: KeyPair,
    }

    impl Signer {
        fn new(ca: &Ca, uri: &str) -> anyhow::Result<Self> {
            let mut p = CertificateParams::new(Vec::<String>::new())?;
            p.subject_alt_names = vec![SanType::URI(uri.try_into()?)];
            p.is_ca = IsCa::ExplicitNoCa;
            p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            p.not_before = date_time_ymd(2026, 1, 1);
            p.not_after = date_time_ymd(2027, 1, 1);
            let key = KeyPair::generate()?;
            let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
            Ok(Self { uri: uri.to_owned(), der, key })
        }

        /// `s` signed, its body naming this signer: its bundle.
        fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            let p = statement::payload(&s)?;
            Ok(statement::bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&statement::to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
        }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Transfer `t`'s thing, by the SHA-256 of `bytes`.
    fn thing(t: usize, bytes: &[u8]) -> Subject {
        Subject { name: format!("thing {t}"), digest: BTreeMap::from([("sha256".to_owned(), sha(bytes))]), ..Subject::default() }
    }

    /// How a relay treats what it carries, drawn as it comes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Way {
        Honest,
        /// It never passes it on, and signs nothing of it.
        DropsSilently,
        /// It signs its pass of it, then never passes it on.
        DropsAfterPass,
        /// It passes it on, and loses its own record of passing it on.
        LosesRecord,
        /// A byte changed, its pass naming what it got (it hides the change).
        ChangesHiding,
        /// A byte changed, its pass naming what it sends (it owns the change).
        ChangesOwning,
        /// It holds it till another thing comes through it, then sends each in the other's place,
        /// its passes naming what it got for each (it hides the swap).
        Swaps,
        /// It sends a thing handed on before in its place.
        Replays,
    }

    /// One thing handed on: from its giver through its relays (`route`) to its taker.
    struct Transfer {
        taker: usize,
        route: Vec<usize>,
        bytes: Vec<u8>,
        /// Its giver's hand-on, by its digest, and its bundle (what goes with the thing).
        hand_on: String,
        handed: Value,
        /// What happened to it on its way, as the model knows it: whether its taker got other bytes
        /// than its giver handed on (a relay changed them, or put another thing's in their place,
        /// and no later one changed them back), and whether a relay dropped it.
        other: bool,
        dropped: bool,
        /// Its taker's receipt, by its digest, and whether the taker used it.
        receipt: Option<String>,
        used: bool,
        /// Handed on once the world was calm: nothing misbehaves on its way.
        calm: bool,
    }

    /// What travels to the next custodian: the thing's bytes, its giver's hand-on and the passes of
    /// the relays it went through, as they were signed.
    #[derive(Clone)]
    struct Carried {
        transfer: usize,
        bytes: Vec<u8>,
        records: Vec<Value>,
        /// Where on its route it is: the next custodian is `route[at]`, or the taker past it.
        at: usize,
    }

    /// What a swapping relay holds till another thing comes through it: the thing, the last record
    /// that came with it, and whom it passes it on to.
    struct Held {
        c: Carried,
        last: Value,
        to: String,
    }

    #[derive(Clone)]
    enum Ev {
        Tick,
        /// A thing reaches a relay or its taker.
        Reach(Box<Carried>),
        /// A custody record reaches kiln.
        Record(Box<Value>),
        /// Relay `.0`, holding transfer `.1`'s thing to swap and finding nothing to swap it with,
        /// passes it on as it came.
        LetGo(usize, usize),
    }

    struct World {
        rng: Rng,
        plant: Plant,
        ca: Ca,
        trace: Trace,
        now: Duration,
        /// When the nemesis stops, when relays start behaving, when the run ends.
        until: Duration,
        calm: Duration,
        end: Duration,
        custodians: Vec<Signer>,
        relays: Vec<usize>,
        /// Each relay's next number.
        numbered: BTreeMap<usize, u64>,
        /// Each relay's passes in the order they were made: (relay, its number if it numbers them,
        /// digest), kept or not.
        passes: Vec<(usize, Option<u64>, String)>,
        transfers: Vec<Transfer>,
        /// What each swapping relay holds.
        held: BTreeMap<usize, Held>,
        /// What kiln took, by digest.
        records: BTreeMap<String, Checked>,
        counts: Counts,
        queue: Queue<Ev>,
        broke: Option<String>,
    }

    impl World {
        fn new(seed: u64, keep: usize, plant: Plant) -> anyhow::Result<Self> {
            let mut rng = Rng::new(seed);
            let ca = Ca::new()?;
            let n = 3 + rng.index(3);
            let r = 1 + rng.index(2);
            let mut custodians = Vec::new();
            for i in 0..n {
                custodians.push(Signer::new(&ca, &format!("kiln://hyper-light/person/ada/machine/m{i}/instance/i{i}"))?);
            }
            let mut relays = Vec::new();
            for j in 0..r {
                relays.push(custodians.len());
                custodians.push(Signer::new(&ca, &format!("kiln://hyper-light/person/ada/machine/relay{j}/instance/r{j}"))?);
            }
            let until = rng.span(Duration::from_secs(5), Duration::from_secs(20));
            let mut w = Self {
                rng,
                plant,
                ca,
                trace: Trace::new(seed, keep),
                now: Duration::ZERO,
                until,
                calm: until + Duration::from_secs(5),
                end: until + Duration::from_secs(20),
                custodians,
                relays,
                numbered: BTreeMap::new(),
                passes: Vec::new(),
                transfers: Vec::new(),
                held: BTreeMap::new(),
                records: BTreeMap::new(),
                counts: Counts::default(),
                queue: Queue::with_max(QUEUE_MAX),
                broke: None,
            };
            w.queue.push(Duration::ZERO, Ev::Tick);
            Ok(w)
        }

        fn at(&self) -> i64 {
            AT + i64::try_from(self.now.as_secs()).unwrap_or(0)
        }

        fn broke(&mut self, why: String) {
            if self.broke.is_none() {
                let now = self.now;
                self.trace.note(|| format!("{now:?}: broke: {why}"));
                self.broke = Some(why);
            }
        }

        /// A record sent to kiln: late, sometimes twice, while the nemesis lasts; a relay's own
        /// (`lossy`) sometimes never.
        fn tell_kiln(&mut self, bundle: Value, lossy: bool) -> bool {
            let nemesis = self.now < self.until;
            if lossy && nemesis && self.rng.chance(60) {
                self.counts.records_lost += 1;
                return false;
            }
            if nemesis && self.rng.chance(100) {
                let again = self.now + self.rng.span(Duration::from_millis(1), Duration::from_secs(3));
                self.queue.push(again, Ev::Record(Box::new(bundle.clone())));
            }
            let late = if nemesis { self.rng.span(Duration::from_millis(1), Duration::from_secs(2)) } else { self.rng.span(Duration::from_millis(1), Duration::from_millis(20)) };
            self.queue.push(self.now + late, Ev::Record(Box::new(bundle)));
            true
        }

        /// A thing on its way to the next custodian of its route.
        fn send(&mut self, c: Carried) {
            let late = self.rng.span(Duration::from_millis(1), Duration::from_millis(300));
            self.queue.push(self.now + late, Ev::Reach(Box::new(c)));
        }

        /// kiln takes `bundle`, as of now: kept by its digest.
        fn take(&mut self, bundle: &Value) {
            if let Ok(c) = statement::verify(bundle, &self.ca.der, &[], self.at()) {
                self.records.insert(c.digest.clone(), c);
            }
        }

        /// The digest of a record.
        fn digest_of(&self, bundle: &Value) -> String {
            statement::verify(bundle, &self.ca.der, &[], self.at()).map(|c| c.digest).unwrap_or_default()
        }

        /// A new thing handed on, from a giver to a taker, through no relay, one or two.
        fn hand_on(&mut self) -> anyhow::Result<()> {
            let givers: Vec<usize> = (0..self.custodians.len()).filter(|i| !self.relays.contains(i)).collect();
            let (Some(&giver), Some(&taker)) = (self.rng.pick(&givers), self.rng.pick(&givers)) else { return Ok(()) };
            if giver == taker {
                return Ok(());
            }
            let hops = self.rng.index(3);
            let mut route: Vec<usize> = Vec::new();
            for _ in 0..hops {
                if let Some(&r) = self.rng.pick(&self.relays)
                    && !route.contains(&r)
                {
                    route.push(r);
                }
            }
            let t = self.transfers.len();
            let len = 16 + self.rng.index(48);
            let bytes: Vec<u8> = (0..len).map(|_| u8::try_from(self.rng.below(256)).unwrap_or(0)).collect();
            let first = route.first().copied().unwrap_or(taker);
            let to = self.custodians.get(first).map(|s| s.uri.clone()).unwrap_or_default();
            let body = Body { to: Some(to), why: "handed on".into(), ..Body::new(Kind::HandedOn, "") };
            let s = Statement { subject: vec![thing(t, &bytes)], ..Statement::new(custody::to_value(&body)) };
            let handed = self.custodians.get(giver).ok_or_else(|| anyhow::anyhow!("no giver"))?.sign(&s)?;
            let hand_on = self.digest_of(&handed);
            let calm = self.now >= self.calm;
            self.transfers.push(Transfer { taker, route, bytes: bytes.clone(), hand_on, handed: handed.clone(), other: false, dropped: false, receipt: None, used: false, calm });
            self.counts.transfers += 1;
            self.tell_kiln(handed.clone(), false);
            self.send(Carried { transfer: t, bytes, records: vec![handed], at: 0 });
            Ok(())
        }

        /// Relay `r`'s pass of what it got (`got`, as it names it) from the last record's custodian
        /// to `to`, following that record; numbered, unless planted not to be.
        fn pass(&mut self, r: usize, t: usize, got: &[u8], last: &Value, to: &str) -> anyhow::Result<Value> {
            let from = statement::verify(last, &self.ca.der, &[], self.at()).ok().and_then(|c| custody::body(&c).ok().map(|b| b.by)).unwrap_or_default();
            let (stream, seq) = if self.plant.unnumbered {
                (None, None)
            } else {
                let n = self.numbered.entry(r).or_insert(0);
                *n += 1;
                (Some(STREAM.to_owned()), Some(*n))
            };
            let body = Body { to: Some(to.to_owned()), from: Some(from), why: "passed on".into(), ..Body::new(Kind::PassedOn, "") };
            let s = Statement { subject: vec![thing(t, got)], follows: vec![self.digest_of(last)], stream, seq, ..Statement::new(custody::to_value(&body)) };
            let bundle = self.custodians.get(r).ok_or_else(|| anyhow::anyhow!("no relay"))?.sign(&s)?;
            let d = self.digest_of(&bundle);
            self.passes.push((r, seq, d));
            Ok(bundle)
        }

        /// A thing reaches the next custodian of its route.
        fn reach(&mut self, mut c: Carried) -> anyhow::Result<()> {
            let Some(tr) = self.transfers.get(c.transfer) else { return Ok(()) };
            let (route, taker, calm) = (tr.route.clone(), tr.taker, tr.calm);
            let Some(&r) = route.get(c.at) else { return self.taken(c, taker) };
            let next = route.get(c.at + 1).copied().unwrap_or(taker);
            let to = self.custodians.get(next).map(|s| s.uri.clone()).unwrap_or_default();
            // A relay holding a thing to swap swaps it with the next that comes while it misbehaves.
            let way = if calm || self.now >= self.calm {
                Way::Honest
            } else if self.held.contains_key(&r) {
                Way::Swaps
            } else {
                self.way()
            };
            let last = c.records.last().cloned().unwrap_or(Value::Null);
            self.counts.relayed += 1;
            let now = self.now;
            self.trace.note(|| format!("{now:?}: relay {r} carries thing {} {way:?}", c.transfer));
            match way {
                Way::Honest => {
                    let p = self.pass(r, c.transfer, &c.bytes, &last, &to)?;
                    self.tell_kiln(p.clone(), true);
                    c.records.push(p);
                }
                Way::DropsSilently => {
                    self.mark(c.transfer, |t| t.dropped = true);
                    self.counts.dropped += 1;
                    return Ok(());
                }
                Way::DropsAfterPass => {
                    let p = self.pass(r, c.transfer, &c.bytes, &last, &to)?;
                    self.tell_kiln(p, true);
                    self.mark(c.transfer, |t| t.dropped = true);
                    self.counts.dropped += 1;
                    return Ok(());
                }
                Way::LosesRecord => {
                    let p = self.pass(r, c.transfer, &c.bytes, &last, &to)?;
                    self.counts.records_lost += 1;
                    c.records.push(p);
                }
                Way::ChangesHiding | Way::ChangesOwning => {
                    let got = c.bytes.clone();
                    if let Some(b) = c.bytes.first_mut() {
                        *b ^= 0x01;
                    }
                    let named = if way == Way::ChangesHiding { got } else { c.bytes.clone() };
                    let p = self.pass(r, c.transfer, &named, &last, &to)?;
                    self.tell_kiln(p.clone(), true);
                    c.records.push(p);
                    self.counts.changed += 1;
                }
                Way::Swaps => {
                    let Some(mut h) = self.held.remove(&r) else {
                        let (t, wait) = (c.transfer, self.rng.span(Duration::from_millis(100), Duration::from_secs(1)));
                        self.held.insert(r, Held { c, last, to });
                        self.queue.push(self.now + wait, Ev::LetGo(r, t));
                        return Ok(());
                    };
                    let (got_held, got) = (h.c.bytes.clone(), c.bytes.clone());
                    std::mem::swap(&mut h.c.bytes, &mut c.bytes);
                    self.counts.swapped += 1;
                    let (now, a) = (self.now, h.c.transfer);
                    self.trace.note(|| format!("{now:?}: relay {r} swaps thing {a} and thing {}", c.transfer));
                    self.passed(r, h.c, &got_held, &h.last, &h.to)?;
                    return self.passed(r, c, &got, &last, &to);
                }
                Way::Replays => {
                    // A thing handed on before, its bytes in this one's place.
                    let before: Vec<usize> = (0..c.transfer).collect();
                    if let Some(&o) = self.rng.pick(&before) {
                        let bytes = self.transfers.get(o).map(|x| x.bytes.clone()).unwrap_or_default();
                        let got = std::mem::replace(&mut c.bytes, bytes);
                        self.counts.replayed += 1;
                        return self.passed(r, c, &got, &last, &to);
                    }
                    let got = c.bytes.clone();
                    return self.passed(r, c, &got, &last, &to);
                }
            }
            self.send(Carried { at: c.at + 1, ..c });
            Ok(())
        }

        /// Relay `r` passes `c` on to `to`, its pass naming what it got (`got`) and following `last`.
        fn passed(&mut self, r: usize, mut c: Carried, got: &[u8], last: &Value, to: &str) -> anyhow::Result<()> {
            let p = self.pass(r, c.transfer, got, last, to)?;
            self.tell_kiln(p.clone(), true);
            c.records.push(p);
            self.send(Carried { at: c.at + 1, ..c });
            Ok(())
        }

        /// How a relay treats what it carries now, while it misbehaves.
        fn way(&mut self) -> Way {
            match self.rng.below(16) {
                0 => Way::DropsSilently,
                1 => Way::DropsAfterPass,
                2 | 3 => Way::LosesRecord,
                4 => Way::ChangesHiding,
                5 => Way::ChangesOwning,
                6 => Way::Swaps,
                7 => Way::Replays,
                _ => Way::Honest,
            }
        }

        fn mark(&mut self, t: usize, f: impl FnOnce(&mut Transfer)) {
            if let Some(x) = self.transfers.get_mut(t) {
                f(x);
            }
        }

        /// The taker checks what came against its giver's hand-on (its signature, then the digest)
        /// before it uses it, and signs its receipt of what came, following the last record.
        fn taken(&mut self, c: Carried, taker: usize) -> anyhow::Result<()> {
            let Some(tr) = self.transfers.get(c.transfer) else { return Ok(()) };
            if tr.receipt.is_some() {
                return Ok(());
            }
            let (handed, hand_on) = (tr.handed.clone(), tr.hand_on.clone());
            // What the giver signed for, as the taker reads it from the hand-on that came: its own,
            // signed, by the digest the taker was told of.
            let signed = c.records.first().and_then(|h| statement::verify(h, &self.ca.der, &[], self.at()).ok());
            let wanted = signed.filter(|h| h.digest == hand_on).and_then(|h| h.statement.subject.first().and_then(|s| s.digest.get("sha256").cloned()));
            let ok = wanted.as_deref() == Some(sha(&c.bytes).as_str());
            let last = c.records.last().cloned().unwrap_or(handed);
            let from = statement::verify(&last, &self.ca.der, &[], self.at()).ok().and_then(|x| custody::body(&x).ok().map(|b| b.by)).unwrap_or_default();
            let body = Body { from: Some(from), why: "received".into(), ..Body::new(Kind::Received, "") };
            let s = Statement { subject: vec![thing(c.transfer, &c.bytes)], follows: vec![self.digest_of(&last)], ..Statement::new(custody::to_value(&body)) };
            let receipt = self.custodians.get(taker).ok_or_else(|| anyhow::anyhow!("no taker"))?.sign(&s)?;
            let d = self.digest_of(&receipt);
            // What it uses is what its giver handed on: bytes changed on the way (and not changed
            // back by a later relay) are never used.
            let other = self.transfers.get(c.transfer).is_some_and(|t| t.bytes != c.bytes);
            if ok && other {
                self.broke(format!("thing {} was used though its bytes aren't its giver's", c.transfer));
            }
            if ok {
                self.counts.used += 1;
            } else {
                self.counts.refused += 1;
            }
            self.mark(c.transfer, |t| {
                t.receipt = Some(d);
                t.used = ok;
                t.other = other;
            });
            self.tell_kiln(receipt, false);
            Ok(())
        }

        /// kiln's walk back from record `from`, as the plant has it.
        fn walked(&self, from: &str) -> Result<custody::Walked, Break> {
            if self.plant.skips_hops {
                return custody::hop(from, &self.records, SERVER).map(|h| custody::Walked { hops: vec![h], ..custody::Walked::default() });
            }
            match custody::walk(from, &self.records, SERVER) {
                Err(Break::Changed { .. }) if self.plant.no_digest_check => Ok(custody::Walked::default()),
                r => r,
            }
        }

        fn run(&mut self) -> anyhow::Result<()> {
            while let Some((at, e)) = self.queue.pop() {
                if at > self.end || self.broke.is_some() {
                    break;
                }
                self.now = at;
                match e {
                    Ev::Tick => {
                        self.trace.fold(1);
                        // Things handed on while the run lasts, till its last few seconds.
                        if self.now + Duration::from_secs(5) < self.end && self.rng.chance(400) {
                            self.hand_on()?;
                        }
                        let next = self.now + self.rng.span(Duration::from_millis(50), Duration::from_millis(400));
                        self.queue.push(next, Ev::Tick);
                    }
                    Ev::Reach(c) => {
                        self.trace.fold(2);
                        self.reach(*c)?;
                    }
                    Ev::Record(b) => {
                        self.trace.fold(3);
                        self.take(&b);
                    }
                    Ev::LetGo(r, t) => {
                        self.trace.fold(4);
                        if self.held.get(&r).is_some_and(|h| h.c.transfer == t)
                            && let Some(h) = self.held.remove(&r)
                        {
                            let got = h.c.bytes.clone();
                            self.passed(r, h.c, &got, &h.last, &h.to)?;
                        }
                    }
                }
            }
            Ok(())
        }

        /// Once the run is over: each chain walked as kiln walks it, held to what happened.
        fn settled(&mut self) -> Result<(), String> {
            let gaps = statement::gaps(self.records.values());
            self.counts.gaps_found += u64::try_from(gaps.len()).unwrap_or(0);
            // Nothing a relay passed on is unseen: its pass kept, or a gap in its numbers where the
            // pass should be, once a later pass of its is kept (a last pass lost has nothing after it
            // to show it yet: the relay's next pass will).
            for (k, (r, n, d)) in self.passes.iter().enumerate() {
                if self.records.contains_key(d) {
                    continue;
                }
                let later = self.passes.iter().skip(k + 1).any(|(r2, _, d2)| r2 == r && self.records.contains_key(d2));
                if !later {
                    continue;
                }
                let uri = self.custodians.get(*r).map(|s| s.uri.as_str()).unwrap_or_default();
                let missing = n.is_some_and(|n| gaps.iter().any(|g| matches!(g, statement::Gap::Missing { custodian, from, to, .. } if custodian == uri && *from <= n && n <= *to)));
                if !missing {
                    return Err(format!("relay {r}'s pass {} never reached kiln, and nothing shows it", n.map_or_else(|| "(unnumbered)".to_owned(), |n| n.to_string())));
                }
            }
            for (i, t) in self.transfers.iter().enumerate() {
                let Some(receipt) = &t.receipt else {
                    if t.calm && !t.dropped {
                        return Err(format!("thing {i}, handed on once calm, never reached its taker"));
                    }
                    continue;
                };
                if !self.records.contains_key(receipt) {
                    return Err(format!("thing {i}'s receipt never reached kiln"));
                }
                match self.walked(receipt) {
                    Ok(w) => {
                        // A change a later relay undid, every record naming its giver's bytes, leaves
                        // nothing to break, and needs nothing: what came is what was handed on.
                        if t.other {
                            return Err(format!("thing {i}'s taker got other bytes than its giver handed on, and its chain walks as unbroken ({} hops)", w.hops.len()));
                        }
                        self.counts.unbroken += 1;
                    }
                    Err(b) => {
                        self.counts.breaks_found += 1;
                        // Handed on once calm, only a gap a relay of its route left before can break it:
                        // a walk holds every record of a custodian's stream to its numbers.
                        if t.calm && !matches!(b, Break::Gap(_)) {
                            return Err(format!("thing {i}, handed on once calm, breaks: {b}"));
                        }
                    }
                }
                if t.calm && !t.used {
                    return Err(format!("thing {i}, handed on once calm, wasn't used"));
                }
            }
            Ok(())
        }
    }

    /// One seed's run, `plant`ed: what it counted and its trace's digest, or what went wrong.
    pub fn run(seed: u64, keep: usize, plant: Plant) -> Result<(Counts, u64), String> {
        let mut w = World::new(seed, keep, plant).map_err(|e| format!("{e:#}"))?;
        let ran = w.run().map_err(|e| format!("{e:#}")).and_then(|()| w.broke.clone().map_or(Ok(()), Err)).and_then(|()| w.settled());
        match ran {
            Ok(()) => Ok((w.counts, w.trace.digest())),
            Err(e) => {
                let tail = if w.trace.keeps() { format!("\n{}", w.trace.tail_text()) } else { String::new() };
                Err(format!("{e}{tail}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kiln_sim::seeds;

    use super::model::{Counts, Plant, run};

    /// W27-R08 (the Transfers plan's step 15): over the seed budget, no chain with a break walks as
    /// unbroken, nothing used differs from what its giver handed on, nothing a relay passed on is
    /// unseen, and once the world calms every thing handed on is used and walks unbroken; and what
    /// the model is about happened.
    #[test]
    fn w27_r08_no_broken_chain_walks_as_unbroken() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("custody", 200, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (counts, digest) = run(seed, keep, Plant::default())?;
            all.add(&counts);
            Ok(digest)
        });
        eprintln!("{}\n  counted: {all:?}", report.line());
        let ran = report.seeds;
        report.into_result()?;
        if seeds::replay().is_none() && ran >= 200 {
            let none = [
                ("things handed on", all.transfers),
                ("things relayed", all.relayed),
                ("things used", all.used),
                ("things refused", all.refused),
                ("bytes changed on their way", all.changed),
                ("things swapped", all.swapped),
                ("things replayed", all.replayed),
                ("things dropped", all.dropped),
                ("relays' records lost", all.records_lost),
                ("breaks found", all.breaks_found),
                ("gaps found", all.gaps_found),
                ("chains walked unbroken", all.unbroken),
            ];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// W27-R08: each bug planted is found within the budget, by a seed that passes without it.
    #[test]
    fn w27_r08_the_custody_model_finds_each_planted_bug() -> Result<(), String> {
        let plants = [
            ("a relay that doesn't number its records", Plant { unnumbered: true, ..Plant::default() }),
            ("a hop that doesn't check the digest", Plant { no_digest_check: true, ..Plant::default() }),
            ("a walk that skips a hop", Plant { skips_hops: true, ..Plant::default() }),
        ];
        let budget = seeds::budget(200);
        for (what, plant) in plants {
            let found = (0..budget).find(|seed| run(*seed, 0, plant).is_err()).ok_or_else(|| format!("{what}: not found in {budget} seeds"))?;
            let why = run(found, 0, plant).err().unwrap_or_default();
            run(found, 0, Plant::default()).map_err(|e| format!("{what}: seed {found} fails without it too: {e}"))?;
            eprintln!("{what}: found by seed {found}: {why}");
        }
        Ok(())
    }

    /// W27-R08: a seed runs the same every time.
    #[test]
    fn w27_r08_a_custody_seed_replays_exactly() -> Result<(), String> {
        for seed in [3, 77, 4113] {
            seeds::twice(seed, |s| run(s, 0, Plant::default()).map(|(_, d)| d))?;
        }
        Ok(())
    }
}
