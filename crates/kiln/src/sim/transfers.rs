//! The transfers model (docs/ci.md W27-R08 and W27-AC05; the Transfers plan's step 9): machines
//! with one or two runner processes (blue/green upgrades, their rollbacks, take-overs), kiln's
//! server (restarts, its count of which process holds a session going stale), and agent sessions
//! changing hands under kiln-sim's seeded nemesis (frames lost, sent twice and late; machines cut
//! off, what their runners send kept until the link comes back). Sessions are handed on, resumed
//! from the server's store while their machine is cut off (kiln's take, for the process that can't
//! hand them on), taken over by a newer process of their machine and carried through upgrades;
//! the copies left behind keep acting at random: acts, hand-ons of their session, announcements.
//!
//! It runs kiln's own code where a hand-over, an act and a word are decided, over statements signed
//! under a test CA: the server's act check (`custody::act_check`: revocations, then
//! `kiln_ci::identity::custody::acted`'s fence), its check of a hand-on (`custody::hands_on_to`),
//! the custody chain's hop and walk (`kiln_ci::identity::custody`), the server's signed words
//! (`custody::word_signed`) and the runner's check of one (`runner::custody::addressed`). It
//! checks:
//! - safety: no act a copy left behind signs is ever applied; no hand-over's word is applied by a
//!   process it isn't for; a session has one holder per epoch; every transfer's records carry both
//!   its sides' signatures (a take: kiln's, for the side that couldn't sign) and walk back to where
//!   the session began;
//! - liveness: once the nemesis has stopped and the world calmed, every session's holder runs it,
//!   and no copy left behind still runs.
//!
//! Left out: the host's own take-over (its package, files and harness: here a take-over is the
//! epoch up, the receipt signed at it, and a session refused where a copy of it runs already), the
//! sessions' data (the `moves` model's), and work, changes and reviews changing hands (tested end to
//! end, `tests/custody.rs`).

#[cfg(test)]
mod model {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::time::Duration;

    use kiln_ci::identity::chain::{Actor, Holder, Identity, Revocation};
    use kiln_ci::identity::custody::{self, Act, Body, Held, Kind, PAYLOAD_ACT};
    use kiln_ci::identity::statement::{self, Checked, PAYLOAD_CUSTODY, Statement, Subject};
    use kiln_sim::{Queue, Rng, Trace};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData as _, SanType, SerialNumber, SigningKey as _, date_time_ymd};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    const ORG: &str = "hyper-light";
    const SERVER: &str = "kiln://hyper-light/server";
    /// 2026-09-21T14:13:20Z, inside every test certificate's validity: a run's clock starts there.
    const AT: i64 = 1_790_000_000;
    /// Frames a runner keeps for the server while its machine is cut off, at most: past it, they're
    /// lost (counted).
    const OUTBOX_MAX: usize = 256;
    /// Events the model's queue holds, at most.
    const QUEUE_MAX: usize = 1 << 16;

    /// Bugs planted on purpose, each one kiln had or could have: the model finds each within its
    /// budget of seeds.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Plant {
        /// The server's act check goes by machine: an act counts from any process of its holder's
        /// machine, at any epoch (kiln before the twin's fix, for an act sent unsigned).
        pub machine_fence: bool,
        /// A word a runner process a newer one superseded passes on to it (as the old runner did in the
        /// 14:18 drop) is applied there as its own, unchecked.
        pub forwards_words: bool,
        /// A resume from the server's store doesn't revoke the epoch the session leaves.
        pub resume_keeps_epoch: bool,
        /// The server tells a process to let go of a session that went on elsewhere only once the
        /// copy left behind there announces it, not at the runner's first word (the twin, which ran
        /// on for ten minutes).
        pub release_on_announce: bool,
        /// A runner process applies a hand-over's word whomever it's for.
        pub no_addressee: bool,
    }

    /// What runs did, counted: that the model tested what it says.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Counts {
        pub moves: u64,
        pub resumes: u64,
        pub take_overs: u64,
        pub blue_greens: u64,
        pub rollbacks: u64,
        pub restarts: u64,
        pub cuts: u64,
        pub acts_applied: u64,
        pub stale_acts_refused: u64,
        pub stale_hand_ons_refused: u64,
        pub words_applied: u64,
        pub words_refused: u64,
        pub let_go: u64,
        pub lost: u64,
    }

    impl Counts {
        pub fn add(&mut self, o: &Self) {
            self.moves += o.moves;
            self.resumes += o.resumes;
            self.take_overs += o.take_overs;
            self.blue_greens += o.blue_greens;
            self.rollbacks += o.rollbacks;
            self.restarts += o.restarts;
            self.cuts += o.cuts;
            self.acts_applied += o.acts_applied;
            self.stale_acts_refused += o.stale_acts_refused;
            self.stale_hand_ons_refused += o.stale_hand_ons_refused;
            self.words_applied += o.words_applied;
            self.words_refused += o.words_refused;
            self.let_go += o.let_go;
            self.lost += o.lost;
        }
    }

    /// The test CA: its certificate, and what issues under it.
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

    /// An identity's key and the certificate the CA issued for it: what signs as it.
    struct Signer {
        uri: String,
        der: Vec<u8>,
        key: KeyPair,
        serial: String,
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
            let serial: Vec<u8> = std::iter::once(0x42).chain(Sha256::digest(key.der_bytes()).iter().take(8).copied()).collect();
            p.serial_number = Some(SerialNumber::from_slice(&serial));
            let der = p.signed_by(&key, &ca.issuer)?.der().to_vec();
            Ok(Self { uri: uri.to_owned(), der, key, serial: hex(&serial) })
        }

        /// `s` signed as `payload_type`, its body naming this signer: its bundle.
        fn sign(&self, payload_type: &str, s: &Statement) -> anyhow::Result<Value> {
            let mut s = s.clone();
            if let Some(o) = s.body.as_object_mut() {
                o.insert("by".into(), Value::String(self.uri.clone()));
            }
            let p = statement::payload(&s)?;
            Ok(statement::bundle(payload_type, &p, &self.key.sign(&statement::to_sign(payload_type, &p))?, &self.der))
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Session `s`'s identity at `epoch`, held by process `held_by` (machine, instance) or by none
    /// named (whichever process takes it: what a hand-on is to).
    fn agent(s: usize, epoch: u64, held_by: Option<(String, String)>) -> anyhow::Result<String> {
        let held_by = held_by.map(|(machine, instance)| Holder { machine, instance });
        Ok(Identity::new(ORG, Actor::Agent { person: "ada".into(), agent: format!("s{s}"), epoch, held_by }).uri()?)
    }

    /// Session `s`'s timeline as it leaves `epoch`: the thing a hand-over of it is about.
    fn timeline(s: usize, epoch: u64) -> Subject {
        Subject { name: format!("session s{s} at epoch {epoch}"), digest: BTreeMap::from([("sha256".to_owned(), hex(&Sha256::digest(format!("s{s}/{epoch}"))))]), ..Subject::default() }
    }

    /// A runner process of a machine.
    struct Proc {
        machine: usize,
        instance: String,
        /// Running: an upgrade's old process exits, its rolled-back new one too, and a process taken
        /// over from once it runs nothing more.
        up: bool,
        /// Taken over from by a newer process of its machine (it didn't answer): it runs on until
        /// what it ran is let go.
        wedged: bool,
    }

    /// A copy of a session that a process runs at an epoch, with the key kiln certified for it then.
    struct Copy {
        session: usize,
        epoch: u64,
        proc: usize,
        key: Signer,
        running: bool,
        /// Its receipt of the session (none at epoch 0, where it began): what its hand-on follows.
        receipt: Option<String>,
    }

    /// A frame from a runner process to the server.
    #[derive(Clone)]
    enum Up {
        /// An act a copy signed, beside the ask it signed (`what`).
        Act { copy: usize, bundle: Value, what: Vec<u8> },
        /// A copy of a session says it runs there.
        Announce { session: usize },
        /// A copy hands its session on to process `to` (one left behind, as if it still held it).
        HandOn { copy: usize, bundle: Value, to: usize },
        /// The runner's beat.
        Beat,
    }

    /// A frame from the server to a runner process.
    #[derive(Clone)]
    enum Down {
        /// A hand-over's word about a session (`json`, the command with the server's word beside it),
        /// for process `meant` at `epoch`; `passed_on` by another process (planted).
        Word { session: usize, json: String, meant: usize, epoch: u64, passed_on: bool },
        /// Let go of every copy of `session` from before `epoch`: it went on elsewhere.
        LetGo { session: usize, epoch: u64 },
    }

    #[derive(Clone)]
    enum Ev {
        /// The world's next move.
        Tick,
        /// A frame reaches the server from process `proc`.
        Up { proc: usize, frame: Up },
        /// A frame reaches process `proc` from the server.
        Down { proc: usize, frame: Down },
        /// A machine's link comes back.
        Heal { machine: usize },
        /// A process's runner beats (every second or two while it's up).
        Beat { proc: usize },
        /// A blue/green upgrade settles: its old process exits, or its new one is rolled back.
        Upgraded { old: usize, new: usize },
    }

    struct World {
        rng: Rng,
        plant: Plant,
        ca: Ca,
        server: Signer,
        trace: Trace,
        now: Duration,
        /// When the nemesis stops (no frame lost, no machine cut off, no restart after it), when the
        /// world calms (no hand-over after it), and when the run ends.
        until: Duration,
        calm: Duration,
        end: Duration,
        machines: usize,
        cut: Vec<bool>,
        /// What each machine's runner keeps for the server while it's cut off.
        outbox: Vec<VecDeque<(usize, Up)>>,
        procs: Vec<Proc>,
        copies: Vec<Copy>,
        /// Each session's newest epoch kiln certified, and the copy it certified then: its holder.
        held: Vec<(u64, usize)>,
        /// The process kiln's count says holds each session: set as it certifies one, and by what
        /// processes announce (copies left behind among them); stale after a restart.
        count: Vec<usize>,
        /// The process kiln certified at each (session, epoch).
        certified: BTreeMap<(usize, u64), usize>,
        /// (process, session): a process a session went on from, which kiln tells to let go.
        left: BTreeSet<(usize, usize)>,
        revoked: Vec<Revocation>,
        records: BTreeMap<String, Checked>,
        counts: Counts,
        queue: Queue<Ev>,
        /// The first thing that went wrong.
        broke: Option<String>,
    }

    impl World {
        fn new(seed: u64, keep: usize, plant: Plant) -> anyhow::Result<Self> {
            let mut rng = Rng::new(seed);
            let ca = Ca::new()?;
            let server = Signer::new(&ca, SERVER)?;
            let machines = 2 + rng.index(2);
            let sessions = 1 + rng.index(3);
            let until = rng.span(Duration::from_secs(10), Duration::from_secs(30));
            let mut w = Self {
                rng,
                plant,
                ca,
                server,
                trace: Trace::new(seed, keep),
                now: Duration::ZERO,
                until,
                calm: until + Duration::from_secs(10),
                end: until + Duration::from_secs(20),
                machines,
                cut: vec![false; machines],
                outbox: (0..machines).map(|_| VecDeque::new()).collect(),
                procs: Vec::new(),
                copies: Vec::new(),
                held: Vec::new(),
                count: Vec::new(),
                certified: BTreeMap::new(),
                left: BTreeSet::new(),
                revoked: Vec::new(),
                records: BTreeMap::new(),
                counts: Counts::default(),
                queue: Queue::with_max(QUEUE_MAX),
                broke: None,
            };
            for m in 0..machines {
                w.start(m);
            }
            for s in 0..sessions {
                let p = w.rng.index(machines);
                let c = w.copy(s, 0, p, None)?;
                w.held.push((0, c));
                w.count.push(p);
                w.certified.insert((s, 0), p);
            }
            w.queue.push(Duration::ZERO, Ev::Tick);
            Ok(w)
        }

        /// A new runner process on machine `m`, beating: its index.
        fn start(&mut self, m: usize) -> usize {
            let p = self.procs.len();
            self.procs.push(Proc { machine: m, instance: format!("m{m}p{p}"), up: true, wedged: false });
            let first = self.now + self.rng.span(Duration::from_millis(100), Duration::from_millis(1500));
            self.queue.push(first, Ev::Beat { proc: p });
            p
        }

        fn machine_of(&self, p: usize) -> usize {
            self.procs.get(p).map_or(0, |x| x.machine)
        }

        fn holder_of(&self, p: usize) -> Option<(String, String)> {
            self.procs.get(p).map(|x| (format!("m{}", x.machine), x.instance.clone()))
        }

        fn instance_of(&self, p: usize) -> String {
            self.procs.get(p).map(|x| x.instance.clone()).unwrap_or_default()
        }

        fn is_cut(&self, m: usize) -> bool {
            self.cut.get(m).copied().unwrap_or(false)
        }

        fn up(&self, p: usize) -> bool {
            self.procs.get(p).is_some_and(|x| x.up)
        }

        /// The newest process running on machine `m`.
        fn newest(&self, m: usize) -> Option<usize> {
            self.procs.iter().rposition(|x| x.up && x.machine == m)
        }

        /// The epoch process `p` runs a copy of session `s` at, if it runs one.
        fn runs(&self, p: usize, s: usize) -> Option<u64> {
            self.copies.iter().filter(|c| c.running && c.proc == p && c.session == s).map(|c| c.epoch).max()
        }

        fn running(&self) -> Vec<usize> {
            (0..self.copies.len()).filter(|i| self.copies.get(*i).is_some_and(|c| c.running)).collect()
        }

        fn holder(&self, s: usize) -> (u64, usize) {
            self.held.get(s).copied().unwrap_or((0, 0))
        }

        /// The sessions process `p` holds.
        fn held_by(&self, p: usize) -> Vec<usize> {
            (0..self.held.len()).filter(|s| self.copies.get(self.holder(*s).1).is_some_and(|c| c.proc == p)).collect()
        }

        fn at(&self) -> i64 {
            AT + i64::try_from(self.now.as_secs()).unwrap_or(0)
        }

        fn broke(&mut self, why: String) {
            let now = self.now;
            self.trace.note(|| format!("{now:?}: BROKE {why}"));
            if self.broke.is_none() {
                self.broke = Some(format!("{now:?}: {why}"));
            }
        }

        /// A copy of session `s` at `epoch` on process `p`, its key certified for it there.
        fn copy(&mut self, s: usize, epoch: u64, p: usize, receipt: Option<String>) -> anyhow::Result<usize> {
            let key = Signer::new(&self.ca, &agent(s, epoch, self.holder_of(p))?)?;
            self.copies.push(Copy { session: s, epoch, proc: p, key, running: true, receipt });
            Ok(self.copies.len() - 1)
        }

        /// Session `s` as kiln's server has it now, for its act check.
        fn held_view(&self, s: usize) -> Held {
            let (epoch, c) = self.holder(s);
            let by = self.copies.get(c).and_then(|c| self.holder_of(c.proc)).map(|(machine, instance)| Holder { machine, instance });
            Held { session: format!("s{s}"), epoch, by, since: self.at(), transfer: String::new() }
        }

        /// A frame from process `p` to the server: kept by its runner while its machine is cut off
        /// (sent when the link comes back), else sent as the nemesis has it.
        fn send_up(&mut self, p: usize, frame: Up) {
            let m = self.machine_of(p);
            if self.is_cut(m) {
                match self.outbox.get_mut(m) {
                    Some(o) if o.len() < OUTBOX_MAX => o.push_back((p, frame)),
                    _ => self.counts.lost += 1,
                }
                return;
            }
            self.deliver(Ev::Up { proc: p, frame });
        }

        /// A frame from the server to process `p`: lost while its machine is cut off.
        fn send_down(&mut self, p: usize, frame: Down) {
            if self.is_cut(self.machine_of(p)) {
                self.counts.lost += 1;
                return;
            }
            self.deliver(Ev::Down { proc: p, frame });
        }

        /// `e` on its way: while the nemesis lasts, maybe lost, maybe twice, late.
        fn deliver(&mut self, e: Ev) {
            let nemesis = self.now < self.until;
            if nemesis && self.rng.chance(50) {
                self.counts.lost += 1;
                return;
            }
            if nemesis && self.rng.chance(80) {
                let again = self.now + self.rng.span(Duration::from_millis(1), Duration::from_millis(2000));
                self.queue.push(again, e.clone());
            }
            let late = if nemesis { self.rng.span(Duration::from_millis(1), Duration::from_millis(1500)) } else { self.rng.span(Duration::from_millis(1), Duration::from_millis(10)) };
            self.queue.push(self.now + late, e);
        }

        /// kiln's server takes custody record `bundle`, as of now, revocations and all: its digest, or
        /// none if it's refused.
        fn take(&mut self, bundle: &Value) -> Option<String> {
            let c = statement::verify(bundle, &self.ca.der, &self.revoked, self.at()).ok()?;
            let d = c.digest.clone();
            self.records.insert(d.clone(), c);
            Some(d)
        }

        /// Copy `from`'s hand-on of its session to the next epoch, signed by it, or kiln's take of it
        /// for that copy (`take`: it can't sign, cut off or not answering).
        fn hand_on(&self, from: usize, take: bool) -> anyhow::Result<Option<Value>> {
            let Some(c) = self.copies.get(from) else { return Ok(None) };
            let (s, next) = (c.session, c.epoch + 1);
            let b = if take {
                Body { to: Some(agent(s, next, None)?), taken_for: Some(c.key.uri.clone()), why: "resumed from kiln's copy".into(), epoch: Some(next), ..Body::new(Kind::Taken, "") }
            } else {
                Body { to: Some(agent(s, next, None)?), why: "handed on".into(), epoch: Some(next), ..Body::new(Kind::HandedOn, "") }
            };
            let st = Statement { subject: vec![timeline(s, c.epoch)], follows: c.receipt.iter().cloned().collect(), ..Statement::new(custody::to_value(&b)) };
            Ok(Some(if take { self.server.sign(PAYLOAD_CUSTODY, &st)? } else { c.key.sign(PAYLOAD_CUSTODY, &st)? }))
        }

        /// kiln's server takes hand-on `given` of copy `from`'s session, to process `to`, as the
        /// process there asks for its key at the next epoch with its receipt: refused (none) if kiln
        /// can't take the hand-on (its signer's identity revoked), the epoch is one the session left
        /// (kiln certifies none there), or a copy of the session runs there already (the host refuses
        /// it). Else both records kept, the epochs below revoked (`revoke`) and the new holder
        /// certified: the copy it runs.
        fn transfer(&mut self, from: usize, given: &Value, to: usize, revoke: bool) -> anyhow::Result<Option<usize>> {
            let Some((s, giver, epoch)) = self.copies.get(from).map(|c| (c.session, c.key.uri.clone(), c.epoch)) else { return Ok(None) };
            let next = epoch + 1;
            if !self.up(to) || self.runs(to, s).is_some() || next < self.holder(s).0 {
                return Ok(None);
            }
            let Some(handed) = self.take(given) else { return Ok(None) };
            let issued = agent(s, next, self.holder_of(to))?;
            let body = self.records.get(&handed).map(custody::body).transpose().map_err(|b| anyhow::anyhow!("{b}"))?;
            if let Some(Err(why)) = body.map(|b| crate::custody::hands_on_to(&b, &issued, next)) {
                self.broke(format!("a hand-on of s{s} isn't to its next epoch: {why}"));
                return Ok(None);
            }
            let c = self.copy(s, next, to, None)?;
            let b = Body { from: Some(giver), why: "taken".into(), epoch: Some(next), ..Body::new(Kind::Received, "") };
            let st = Statement { subject: vec![timeline(s, epoch)], follows: vec![handed.clone()], ..Statement::new(custody::to_value(&b)) };
            let receipt = match self.copies.get(c) {
                Some(copy) => copy.key.sign(PAYLOAD_CUSTODY, &st)?,
                None => return Ok(None),
            };
            let Some(got) = self.take(&receipt) else {
                self.broke(format!("kiln refused the receipt of s{s} at epoch {next} by the process it certifies"));
                return Ok(None);
            };
            for d in [&handed, &got] {
                if let Err(b) = custody::hop(d, &self.records, SERVER) {
                    self.broke(format!("a transfer of s{s} to epoch {next} breaks at its hop: {b}"));
                }
            }
            if let Some(copy) = self.copies.get_mut(c) {
                copy.receipt = Some(got);
            }
            if revoke {
                let at = self.at();
                let below: Vec<String> = self.copies.iter().filter(|x| x.session == s && x.epoch < next).map(|x| x.key.serial.clone()).collect();
                for serial in below {
                    if !self.revoked.iter().any(|r| r.serial.as_deref() == Some(serial.as_str())) {
                        self.revoked.push(Revocation { serial: Some(serial), key: None, at, reason: format!("s{s}'s epoch left: moved on to epoch {next}"), by: "kiln".into() });
                    }
                }
            }
            // One holder per epoch: kiln certifies one process at each.
            if let Some(other) = self.certified.get(&(s, next)).copied().filter(|p| *p != to) {
                let (a, b) = (self.instance_of(other), self.instance_of(to));
                self.broke(format!("session s{s} has two holders at epoch {next}: {a} and {b}"));
            }
            self.certified.insert((s, next), to);
            if next > self.holder(s).0
                && let Some(h) = self.held.get_mut(s)
            {
                *h = (next, c);
            }
            if let Some(n) = self.count.get_mut(s) {
                *n = to;
            }
            if let Some(p) = self.copies.get(from).map(|x| x.proc).filter(|p| *p != to) {
                self.left.insert((p, s));
            }
            self.left.remove(&(to, s));
            self.trace.fold(0x7a);
            self.trace.fold(next);
            let (now, on) = (self.now, self.instance_of(to));
            self.trace.note(|| format!("{now:?}: s{s} to epoch {next} on {on}"));
            Ok(Some(c))
        }

        /// Session `s` handed on by its holder (or kiln's take for it, `take`) to process `to`; the
        /// holder lets go of it if it handed it on itself.
        fn hand_over(&mut self, s: usize, to: usize, take: bool, revoke: bool) -> anyhow::Result<bool> {
            let (_, from) = self.holder(s);
            let Some(given) = self.hand_on(from, take)? else { return Ok(false) };
            if self.transfer(from, &given, to, revoke)?.is_none() {
                return Ok(false);
            }
            if !take && let Some(c) = self.copies.get_mut(from) {
                c.running = false;
            }
            Ok(true)
        }

        /// The server says a word about session `s` for the process it certified at its epoch, sent
        /// to the one its count names.
        fn word(&mut self, s: usize) -> anyhow::Result<()> {
            let (epoch, c) = self.holder(s);
            let Some(meant) = self.copies.get(c).map(|c| c.proc) else { return Ok(()) };
            let Some((machine, instance)) = self.holder_of(meant) else { return Ok(()) };
            let command = json!({ "type": "handed_over", "session": format!("s{s}"), "epoch": epoch }).to_string();
            let json = crate::custody::word_signed(&|t, st| self.server.sign(t, st), &format!("s{s}"), &command, &machine, &instance, epoch)?;
            let to = self.count.get(s).copied().unwrap_or(meant);
            self.send_down(to, Down::Word { session: s, json, meant, epoch, passed_on: false });
            Ok(())
        }

        /// A word reaches process `p`: applied if it's that process's (kiln's runner checks), else
        /// refused. One a newer process superseded on its machine passes it on to that one half the
        /// time, as the 14:18 drop's old runner did. Planted: a word passed on is applied unchecked; or
        /// every process applies every word.
        fn on_word(&mut self, p: usize, s: usize, json: String, meant: usize, epoch: u64, passed_on: bool) -> anyhow::Result<()> {
            let newest = self.newest(self.machine_of(p)).filter(|n| *n != p);
            if let Some(n) = newest.filter(|_| !passed_on)
                && self.rng.chance(500)
            {
                self.queue.push(self.now + Duration::from_millis(1), Ev::Down { proc: n, frame: Down::Word { session: s, json, meant, epoch, passed_on: true } });
                return Ok(());
            }
            let here = self.runs(p, s);
            let Some((machine, instance)) = self.holder_of(p) else { return Ok(()) };
            let v: Value = serde_json::from_str(&json)?;
            let signed = v.get("word").and_then(Value::as_str).unwrap_or_default();
            let checked = crate::runner::custody::addressed(&format!("s{s}"), &v, signed, &self.ca.der, &machine, &instance, here, self.at());
            if let Err(why) = checked
                && !(passed_on && self.plant.forwards_words)
                && !self.plant.no_addressee
            {
                self.counts.words_refused += 1;
                let now = self.now;
                self.trace.note(|| format!("{now:?}: a word about s{s}{} refused by {instance}: {why}", if passed_on { " passed on" } else { "" }));
                return Ok(());
            }
            self.counts.words_applied += 1;
            // Its process's, at the epoch it holds the session at there (or at none: it let go).
            if p != meant || here.is_some_and(|e| e != epoch) {
                let held = here.map_or_else(|| "at no epoch".to_owned(), |e| format!("at epoch {e}"));
                let for_whom = self.instance_of(meant);
                self.broke(format!("a word about s{s} meant for {for_whom} at epoch {epoch} was applied by {instance}, which holds it {held}"));
            }
            Ok(())
        }

        /// A running copy acts: signed at its epoch, beside the ask it signs.
        fn act(&mut self, c: usize) -> anyhow::Result<()> {
            let op = ["call goals.done", "call agents.send", "call ci.runs.start", "run on a machine", "call show_page"].get(self.rng.index(5)).copied().unwrap_or_default();
            let Some(copy) = self.copies.get(c).filter(|x| x.running) else { return Ok(()) };
            let ask = json!({ "op": op, "session": format!("s{}", copy.session), "at": self.now.as_millis().to_string() }).to_string().into_bytes();
            let body = serde_json::to_value(Act { v: custody::VERSION, by: String::new(), session: format!("s{}", copy.session), epoch: copy.epoch, op: op.to_owned() })?;
            let subject = Subject { name: format!("an act of session s{}", copy.session), digest: BTreeMap::from([("sha256".to_owned(), hex(&Sha256::digest(&ask)))]), ..Subject::default() };
            let bundle = copy.key.sign(PAYLOAD_ACT, &Statement { subject: vec![subject], ..Statement::new(body) })?;
            let p = copy.proc;
            self.send_up(p, Up::Act { copy: c, bundle, what: ask });
            Ok(())
        }

        /// An act reaches the server: applied if kiln's act check takes it (planted: if it's from the
        /// holder's machine).
        fn on_act(&mut self, c: usize, bundle: &Value, what: &[u8]) {
            let Some((s, epoch, proc)) = self.copies.get(c).map(|x| (x.session, x.epoch, x.proc)) else { return };
            let (now_at, holder) = self.holder(s);
            let checked = crate::custody::act_check(bundle, &self.ca.der, &self.revoked, &self.held_view(s), what, self.at());
            let applied = if self.plant.machine_fence { self.copies.get(holder).is_some_and(|h| self.machine_of(h.proc) == self.machine_of(proc)) } else { checked.is_ok() };
            let stale = holder != c;
            if let (false, Err((_, why))) = (applied, &checked) {
                let (now, on) = (self.now, self.instance_of(proc));
                let op = serde_json::from_slice::<Value>(what).ok().and_then(|v| v.get("op").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
                self.trace.note(|| format!("{now:?}: s{s}'s copy at epoch {epoch} on {on}: {op}: refused: {why}"));
            }
            self.trace.fold(if applied { 0xa1 } else { 0xa0 });
            match (applied, stale) {
                (true, false) => self.counts.acts_applied += 1,
                (false, true) => self.counts.stale_acts_refused += 1,
                (true, true) => self.broke(format!("an act of s{s} signed at epoch {epoch}, from a copy left behind (the session is at epoch {now_at}), was applied")),
                (false, false) => self.broke(format!("an act of s{s} from its holder at epoch {epoch} was refused")),
            }
        }

        /// The server hears from process `p` (`announced`: a copy there said it runs that session):
        /// it tells it to let go of every session that went on from it elsewhere (planted: only of
        /// one a copy there announces).
        fn heard(&mut self, p: usize, announced: Option<usize>) {
            let sessions: Vec<usize> = self.left.iter().filter(|(q, s)| *q == p && (!self.plant.release_on_announce || announced == Some(*s))).map(|(_, s)| *s).collect();
            for s in sessions {
                let epoch = self.holder(s).0;
                self.send_down(p, Down::LetGo { session: s, epoch });
            }
        }

        /// A frame reaches the server from process `p`.
        fn at_server(&mut self, p: usize, frame: Up) -> anyhow::Result<()> {
            let mut announced = None;
            match frame {
                Up::Act { copy, bundle, what } => self.on_act(copy, &bundle, &what),
                Up::Announce { session } => {
                    announced = Some(session);
                    if let Some(n) = self.count.get_mut(session) {
                        *n = p;
                    }
                }
                // A copy left behind hands its session on: kiln must refuse it.
                Up::HandOn { copy, bundle, to } => {
                    if self.transfer(copy, &bundle, to, true)?.is_none() {
                        self.counts.stale_hand_ons_refused += 1;
                    }
                }
                Up::Beat => {}
            }
            self.heard(p, announced);
            Ok(())
        }

        /// A frame reaches process `p` from the server.
        fn at_runner(&mut self, p: usize, frame: Down) -> anyhow::Result<()> {
            if !self.up(p) {
                return Ok(());
            }
            match frame {
                Down::Word { session, json, meant, epoch, passed_on } => self.on_word(p, session, json, meant, epoch, passed_on)?,
                Down::LetGo { session, epoch } => {
                    for c in self.copies.iter_mut().filter(|c| c.running && c.proc == p && c.session == session && c.epoch < epoch) {
                        c.running = false;
                        self.counts.let_go += 1;
                    }
                    self.exit_if_done(p);
                }
            }
            Ok(())
        }

        /// A process taken over from exits once it runs nothing.
        fn exit_if_done(&mut self, p: usize) {
            let done = !self.copies.iter().any(|c| c.running && c.proc == p);
            if let Some(x) = self.procs.get_mut(p).filter(|x| x.wedged && done) {
                x.up = false;
            }
        }

        /// The world's next move, at random.
        fn tick(&mut self) -> anyhow::Result<()> {
            let s = self.rng.index(self.held.len());
            let roll = self.rng.below(100);
            let (nemesis, moving) = (self.now < self.until, self.now < self.calm);
            match roll {
                // A running copy acts, left behind or not.
                0..=29 => {
                    if let Some(&c) = self.rng.pick(&self.running()) {
                        self.act(c)?;
                    }
                }
                // Moved to another machine: handed on by its holder, or, its machine cut off, resumed
                // from kiln's copy, the holder left behind there.
                30..=41 if moving => {
                    let (_, holder) = self.holder(s);
                    let from = self.copies.get(holder).map_or(0, |c| self.machine_of(c.proc));
                    let m = (from + 1 + self.rng.index(self.machines - 1)) % self.machines;
                    let Some(to) = self.newest(m).filter(|_| !self.is_cut(m)) else { return Ok(()) };
                    if self.is_cut(from) {
                        let revoke = !self.plant.resume_keeps_epoch;
                        if self.hand_over(s, to, true, revoke)? {
                            self.counts.resumes += 1;
                            let now = self.now;
                            self.trace.note(|| format!("{now:?}: s{s} resumed from kiln's copy, m{from} cut off"));
                        }
                    } else if self.hand_over(s, to, false, true)? {
                        self.counts.moves += 1;
                    }
                }
                // A copy left behind hands its session on as if it still held it.
                42..=46 => {
                    let stale: Vec<usize> = self.running().into_iter().filter(|c| self.copies.get(*c).is_some_and(|x| self.holder(x.session).1 != *c)).collect();
                    if let Some(&c) = self.rng.pick(&stale)
                        && let Some(given) = self.hand_on(c, false)?
                    {
                        let from = self.copies.get(c).map_or(0, |x| self.machine_of(x.proc));
                        let m = (from + 1 + self.rng.index(self.machines - 1)) % self.machines;
                        if let (Some(to), Some(p)) = (self.newest(m), self.copies.get(c).map(|x| x.proc)) {
                            self.send_up(p, Up::HandOn { copy: c, bundle: given, to });
                        }
                    }
                }
                // A newer process of the holder's machine takes over from one that doesn't answer
                // (kiln's take for it; its copies run on, left behind), or a blue/green upgrade hands
                // its sessions on to its new build.
                47..=55 if moving => {
                    let (_, holder) = self.holder(s);
                    let Some(old) = self.copies.get(holder).map(|c| c.proc) else { return Ok(()) };
                    let m = self.machine_of(old);
                    if self.is_cut(m) || self.procs.iter().filter(|x| x.up && x.machine == m).count() > 1 {
                        return Ok(());
                    }
                    let blue_green = roll >= 52;
                    let new = self.start(m);
                    for x in self.held_by(old) {
                        self.hand_over(x, new, !blue_green, true)?;
                    }
                    if !blue_green && let Some(x) = self.procs.get_mut(old) {
                        x.wedged = true;
                    }
                    if blue_green {
                        self.counts.blue_greens += 1;
                        let settles = self.now + self.rng.span(Duration::from_millis(500), Duration::from_secs(5));
                        self.queue.push(settles, Ev::Upgraded { old, new });
                    } else {
                        self.counts.take_overs += 1;
                    }
                    let (now, what) = (self.now, if blue_green { "blue/green" } else { "take-over" });
                    let (a, b) = (self.instance_of(old), self.instance_of(new));
                    self.trace.note(|| format!("{now:?}: {what} on m{m}: {a} to {b}"));
                }
                // The server says a word about the session.
                56..=67 => self.word(s)?,
                // A running copy announces its session.
                68..=75 => {
                    if let Some(&c) = self.rng.pick(&self.running())
                        && let Some((p, session)) = self.copies.get(c).map(|x| (x.proc, x.session))
                    {
                        self.send_up(p, Up::Announce { session });
                    }
                }
                // The nemesis cuts a machine off for a while.
                76..=81 if nemesis => {
                    let m = self.rng.index(self.machines);
                    if !self.is_cut(m)
                        && let Some(x) = self.cut.get_mut(m)
                    {
                        *x = true;
                        self.counts.cuts += 1;
                        let back = self.now + self.rng.span(Duration::from_millis(500), Duration::from_secs(8));
                        self.queue.push(back.min(self.until), Ev::Heal { machine: m });
                        let now = self.now;
                        self.trace.note(|| format!("{now:?}: m{m} cut off"));
                    }
                }
                // The server restarts: its count of who holds what read back stale.
                82..=83 if nemesis => {
                    self.counts.restarts += 1;
                    for x in 0..self.held.len() {
                        let ever: Vec<usize> = self.certified.iter().filter(|((y, _), _)| *y == x).map(|(_, p)| *p).collect();
                        if let Some(&p) = self.rng.pick(&ever)
                            && let Some(n) = self.count.get_mut(x)
                        {
                            *n = p;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
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
                        self.tick()?;
                        let next = self.now + self.rng.span(Duration::from_millis(20), Duration::from_millis(400));
                        self.queue.push(next, Ev::Tick);
                    }
                    Ev::Up { proc, frame } => {
                        self.trace.fold(2);
                        self.at_server(proc, frame)?;
                    }
                    Ev::Down { proc, frame } => {
                        self.trace.fold(3);
                        self.at_runner(proc, frame)?;
                    }
                    Ev::Heal { machine } => {
                        self.trace.fold(4);
                        if let Some(x) = self.cut.get_mut(machine) {
                            *x = false;
                        }
                        let kept: Vec<(usize, Up)> = self.outbox.get_mut(machine).map(|o| o.drain(..).collect()).unwrap_or_default();
                        for (proc, frame) in kept {
                            self.deliver(Ev::Up { proc, frame });
                        }
                        let now = self.now;
                        self.trace.note(|| format!("{now:?}: m{machine} back"));
                    }
                    Ev::Beat { proc } => {
                        if self.up(proc) {
                            self.send_up(proc, Up::Beat);
                            let next = self.now + self.rng.span(Duration::from_millis(1000), Duration::from_millis(2000));
                            self.queue.push(next, Ev::Beat { proc });
                        }
                    }
                    Ev::Upgraded { old, new } => {
                        self.trace.fold(5);
                        self.upgraded(old, new)?;
                    }
                }
            }
            Ok(())
        }

        /// A blue/green upgrade settles: its new build rolled back (its sessions handed back to the
        /// old, which goes on), or its old process exits. Its machine cut off, later.
        fn upgraded(&mut self, old: usize, new: usize) -> anyhow::Result<()> {
            let m = self.machine_of(new);
            if self.is_cut(m) {
                self.queue.push(self.now + Duration::from_secs(1), Ev::Upgraded { old, new });
                return Ok(());
            }
            let rollback = self.rng.chance(300);
            let (keeps, goes) = if rollback { (old, new) } else { (new, old) };
            for x in self.held_by(goes) {
                if !self.hand_over(x, keeps, false, true)? {
                    // Not handed back: the process holding it goes on.
                    return Ok(());
                }
            }
            if rollback {
                self.counts.rollbacks += 1;
            }
            for c in self.copies.iter_mut().filter(|c| c.proc == goes) {
                c.running = false;
            }
            if let Some(x) = self.procs.get_mut(goes) {
                x.up = false;
            }
            let now = self.now;
            self.trace.note(|| format!("{now:?}: upgrade on m{m} {}", if rollback { "rolled back" } else { "done" }));
            Ok(())
        }

        /// What must hold once the world has calmed: every session's holder runs it, no copy left
        /// behind still runs, and every session's chain of transfers walks back to where it began,
        /// each with both sides.
        fn settled(&self) -> Result<(), String> {
            for (s, (epoch, h)) in self.held.iter().enumerate() {
                let holds = self.copies.get(*h).is_some_and(|c| c.running && c.epoch == *epoch && self.up(c.proc));
                if !holds {
                    return Err(format!("session s{s}'s holder at epoch {epoch} doesn't run it once the world calmed"));
                }
                if let Some(c) = self.copies.iter().enumerate().find(|(i, c)| c.running && c.session == s && i != h).map(|(_, c)| c) {
                    let on = self.instance_of(c.proc);
                    return Err(format!("a copy of s{s} left behind at epoch {} on {on} still runs once the world calmed (the session is at epoch {epoch})", c.epoch));
                }
                let Some(receipt) = self.copies.get(*h).and_then(|c| c.receipt.clone()) else { continue };
                let walked = custody::walk(&receipt, &self.records, SERVER).map_err(|b| format!("s{s}'s transfers don't walk: {b}"))?;
                // Each epoch's timeline is a thing of its own, made from the one before: epoch 0's hand-on
                // alone follows nothing.
                let gives = walked.hops.iter().filter(|x| x.kind.gives()).count();
                let takes = walked.hops.iter().filter(|x| x.kind == Kind::Received).count();
                let first = walked.hops.iter().filter(|x| x.kind.gives() && x.made_from.is_empty()).count();
                if u64::try_from(takes).ok() != Some(*epoch) || gives != takes || first != 1 {
                    return Err(format!("s{s}'s transfers to epoch {epoch} walk as {takes} receipts and {gives} hand-ons, {first} of them made from nothing before"));
                }
            }
            if let Some(d) = custody::unanswered(&self.records).first() {
                return Err(format!("a hand-on kiln took has no receipt: {d}"));
            }
            Ok(())
        }
    }

    /// One seed's run, `plant`ed: what it counted and its trace's digest, or what went wrong.
    pub fn run(seed: u64, keep: usize, plant: Plant) -> Result<(Counts, u64), String> {
        let mut w = World::new(seed, keep, plant).map_err(|e| format!("{e:#}"))?;
        let ran = w.run().map_err(|e| format!("{e:#}")).and_then(|()| w.broke.clone().map_or(Ok(()), Err)).and_then(|()| w.settled());
        match ran {
            Ok(()) => {
                if w.trace.keeps() {
                    eprintln!("seed {seed}'s last steps:\n{}", w.trace.tail_text());
                }
                Ok((w.counts, w.trace.digest()))
            }
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

    /// W27-AC05 (the Transfers plan's step 9): over the seed budget, no act a copy left behind signs
    /// is ever applied, no hand-over's word is applied by a process it isn't for, a session has one
    /// holder per epoch, every transfer has both its sides' signatures and walks, and once the world
    /// calms every holder runs its session and no copy left behind still runs; and what the model is
    /// about happened.
    #[test]
    fn w27_ac05_no_stale_act_is_ever_applied() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("transfers", 200, |seed| {
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
                ("moves", all.moves),
                ("resumes from kiln's copy", all.resumes),
                ("take-overs", all.take_overs),
                ("blue/green upgrades", all.blue_greens),
                ("rollbacks", all.rollbacks),
                ("server restarts", all.restarts),
                ("machines cut off", all.cuts),
                ("acts applied", all.acts_applied),
                ("acts of copies left behind refused", all.stale_acts_refused),
                ("hand-ons of copies left behind refused", all.stale_hand_ons_refused),
                ("words applied", all.words_applied),
                ("words refused", all.words_refused),
                ("copies let go", all.let_go),
                ("frames lost", all.lost),
            ];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// W27-AC05: each bug planted is found within the budget, by a seed that passes without it.
    #[test]
    fn w27_ac05_the_transfers_model_finds_each_planted_bug() -> Result<(), String> {
        let plants = [
            ("an act check by machine", Plant { machine_fence: true, ..Plant::default() }),
            ("a superseded process passing words on (the 14:18 drop)", Plant { forwards_words: true, ..Plant::default() }),
            ("a resume that keeps the epoch it leaves", Plant { resume_keeps_epoch: true, ..Plant::default() }),
            ("a release only once the copy left behind announces itself (the twin)", Plant { release_on_announce: true, ..Plant::default() }),
            ("no addressee check", Plant { no_addressee: true, ..Plant::default() }),
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

    /// W27-AC05: a seed runs the same every time.
    #[test]
    fn w27_ac05_a_transfers_seed_replays_exactly() -> Result<(), String> {
        for seed in [3, 77, 4113] {
            seeds::twice(seed, |s| run(s, 0, Plant::default()).map(|(_, d)| d))?;
        }
        Ok(())
    }
}
