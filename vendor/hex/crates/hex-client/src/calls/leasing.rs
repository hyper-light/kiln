//! A connection's lessors (joint plan step 33; docs/design/credential-grants.md C7): its orchestrator's
//! [`hex_api::lease::Lessors`], answering the node's asks for the Shards image VMs this connection creates. Each run's
//! lessor is made as the node asks for it and kept until the node says the run has ended; each lease handed over is kept
//! by name until the node gives it back. When the connection ends, by either side, every lease handed over and not given
//! back is given back here, and every run's lessor let go, so no lease outlives the connection that carried it.
//!
//! The connection is read by a thread of its own from [`super::Core::with_lessors`] on (the event form's loop): the
//! node may ask at any time, with no call of this client's waiting. Each ask is answered on a thread of its own, since a
//! lessor's broker may take its time; each answer is owed to the node ahead of every later request ([`super::Owed`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use hex_api::lease::{LeaseRefused, Lessor, Lessors, SignInRefused, SignInTokens};
use hex_api::{BoxId, Refusal};
use hex_core::wire::{Answer, Request};

use super::{SharedOwner, Transport, poisoned};

/// What the node asked or told, as it came, for the lessors' thread to act on.
pub(crate) struct Heard {
    id: u64,
    answer: Answer,
}

/// A connection's lessors, and what of theirs it holds.
pub(crate) struct Leasing {
    lessors: Arc<dyn Lessors>,
    held: Mutex<Held>,
    /// What the node asked or told since the lessors' thread last looked, in order.
    heard: Mutex<VecDeque<Heard>>,
}

#[derive(Default)]
struct Held {
    /// Each run's lessor, by its id on the connection (its lessor's ask's), with the leases handed over and not given
    /// back.
    runs: BTreeMap<u64, Run>,
    /// The connection has ended: a lease or lessor made after is given back or let go at once.
    ended: bool,
}

struct Run {
    lessor: Arc<dyn Lessor>,
    leases: BTreeSet<String>,
}

impl Leasing {
    pub(crate) fn new(lessors: Arc<dyn Lessors>) -> Arc<Self> {
        Arc::new(Self {
            lessors,
            held: Mutex::new(Held::default()),
            heard: Mutex::new(VecDeque::new()),
        })
    }

    /// The node asked or told `answer`, its header's id `id`: kept for the lessors' thread, read in order.
    pub(crate) fn hear(&self, id: u64, answer: Answer) -> Result<(), Refusal> {
        self.heard
            .lock()
            .map_err(poisoned)?
            .push_back(Heard { id, answer });
        Ok(())
    }

    /// Acts on what the node asked or told since last: each ask answered on a thread of its own, each give-back and
    /// run's end at once.
    pub(crate) fn act<T: Transport>(self: &Arc<Self>, shared: &SharedOwner<T>) {
        let heard = match self.heard.lock() {
            Ok(mut heard) => std::mem::take(&mut *heard),
            Err(_) => return,
        };
        for Heard { id, answer } in heard {
            match answer {
                Answer::LessorAsk(box_id) => self.answering(shared, move |leasing, shared| {
                    leasing.lessor_asked(shared, id, &box_id);
                }),
                Answer::LeaseAsk {
                    run,
                    reference,
                    key,
                } => self.answering(shared, move |leasing, shared| {
                    leasing.lease_asked(shared, id, run, &reference, key);
                }),
                Answer::LeaseGiven(lease) => self.given_back(id, &lease),
                Answer::LessorEnded => self.run_ended(id),
                Answer::CaptureAsk {
                    run,
                    sign_in,
                    tokens,
                    key,
                } => self.answering(shared, move |leasing, shared| {
                    leasing.capture_asked(shared, id, run, &sign_in, tokens, key);
                }),
                Answer::RefreshAsk { run, sign_in, key } => {
                    self.answering(shared, move |leasing, shared| {
                        leasing.refresh_asked(shared, id, run, &sign_in, key);
                    });
                }
                Answer::SignInRevoked { run, sign_in, key } => {
                    self.answering(shared, move |leasing, _| {
                        if let Some(lessor) = leasing.lessor_of(run) {
                            lessor.revoked(&sign_in, key);
                        }
                    });
                }
                _ => {}
            }
        }
    }

    /// `answer` made on a thread of its own: a lessor's broker may take its time, and the connection is read meanwhile.
    fn answering<T: Transport>(
        self: &Arc<Self>,
        shared: &SharedOwner<T>,
        answer: impl FnOnce(&Arc<Self>, &SharedOwner<T>) + Send + 'static,
    ) {
        let (leasing, owner) = (Arc::clone(self), SharedOwner::clone(shared));
        let spawned = std::thread::Builder::new()
            .name("hex-client-lease".to_owned())
            .spawn(move || answer(&leasing, &owner));
        if spawned.is_err() {
            // No thread to answer on: the connection can't keep its promise to answer, so it ends, and the node refuses
            // the ask by the end, typed, as every other waiting on it.
            shared.transport.shutdown();
        }
    }

    /// Box `id`'s run (`run`, the ask's id) asks for its lessor: made by this connection's lessors, kept until the run's
    /// end, or the refusal said.
    fn lessor_asked<T: Transport>(&self, shared: &SharedOwner<T>, run: u64, id: &BoxId) {
        let made = self.lessors.lessor(id);
        let said = match made {
            Ok(lessor) => {
                let kept = self.held.lock().is_ok_and(|mut held| {
                    if held.ended {
                        return false;
                    }
                    held.runs.insert(
                        run,
                        Run {
                            lessor,
                            leases: BTreeSet::new(),
                        },
                    );
                    true
                });
                if !kept {
                    return;
                }
                Ok(())
            }
            Err(why) => Err(why),
        };
        let _ = shared.owe_word(run, &Request::Lessor(said));
    }

    /// Run `run`'s lease of `reference` (ask `ask`): leased by its lessor and handed over, or refused, Shards' kinds.
    fn lease_asked<T: Transport>(
        &self,
        shared: &SharedOwner<T>,
        ask: u64,
        run: u64,
        reference: &str,
        key: Option<[u8; 32]>,
    ) {
        let lessor = self
            .held
            .lock()
            .ok()
            .and_then(|held| held.runs.get(&run).map(|r| Arc::clone(&r.lessor)));
        let Some(lessor) = lessor else {
            let _ = shared.owe_word(
                ask,
                &Request::Leased {
                    run,
                    answer: Err(LeaseRefused::Unavailable(format!(
                        "no lessor of run {run} is held on this connection"
                    ))),
                },
            );
            return;
        };
        let leased = match lessor.lease(reference, key) {
            Ok(lease) => lease,
            Err(refused) => {
                let _ = shared.owe_word(
                    ask,
                    &Request::Leased {
                        run,
                        answer: Err(refused),
                    },
                );
                return;
            }
        };
        let name = leased.lease.clone();
        // Kept by name before it goes, so a give-back or the connection's end finds it: one that comes after the run's
        // end, or the connection's, is given straight back.
        let kept = self.held.lock().is_ok_and(|mut held| {
            if held.ended {
                return false;
            }
            match held.runs.get_mut(&run) {
                Some(r) => r.leases.insert(name.clone()),
                None => false,
            }
        });
        if !kept {
            lessor.release(&name);
            return;
        }
        let owed = shared.owe_word(
            ask,
            &Request::Leased {
                run,
                answer: Ok(leased),
            },
        );
        if owed.is_err() {
            // It never reached the node: given back here, unless the connection's end gave it back first.
            self.given_back(run, &name);
        }
    }

    /// Run `run`'s lessor, while the connection holds it.
    fn lessor_of(&self, run: u64) -> Option<Arc<dyn Lessor>> {
        self.held
            .lock()
            .ok()
            .and_then(|held| held.runs.get(&run).map(|r| Arc::clone(&r.lessor)))
    }

    /// Run `run`'s capture of sign-in `sign_in` (ask `ask`; docs/design/credential-grants.md C8): its tokens handed whole
    /// to the run's lessor, and its placeholders or its refusal answered.
    fn capture_asked<T: Transport>(
        &self,
        shared: &SharedOwner<T>,
        ask: u64,
        run: u64,
        sign_in: &str,
        tokens: SignInTokens,
        key: Option<[u8; 32]>,
    ) {
        let answer = match self.lessor_of(run) {
            Some(lessor) => lessor.capture(sign_in, tokens, key),
            None => Err(no_lessor(run)),
        };
        let _ = shared.owe_word(ask, &Request::Captured { run, answer });
    }

    /// Run `run`'s refresh of sign-in `sign_in` (ask `ask`; C8): its lessor's token response, or its refusal, answered.
    fn refresh_asked<T: Transport>(
        &self,
        shared: &SharedOwner<T>,
        ask: u64,
        run: u64,
        sign_in: &str,
        key: Option<[u8; 32]>,
    ) {
        let answer = match self.lessor_of(run) {
            Some(lessor) => lessor.refresh(sign_in, key),
            None => Err(no_lessor(run)),
        };
        let _ = shared.owe_word(ask, &Request::Refreshed { run, answer });
    }

    /// The node gave back run `run`'s lease `lease`: given back to the run's lessor, once.
    fn given_back(&self, run: u64, lease: &str) {
        let lessor = self.held.lock().ok().and_then(|mut held| {
            let r = held.runs.get_mut(&run)?;
            r.leases.remove(lease).then(|| Arc::clone(&r.lessor))
        });
        if let Some(lessor) = lessor {
            lessor.release(lease);
        }
    }

    /// Run `run` has ended: its lessor let go, any lease of it not given back given back first.
    fn run_ended(&self, run: u64) {
        let ended = self
            .held
            .lock()
            .ok()
            .and_then(|mut held| held.runs.remove(&run));
        if let Some(Run { lessor, leases }) = ended {
            for lease in &leases {
                lessor.release(lease);
            }
        }
    }

    /// The connection has ended: every lease handed over and not given back is given back, every run's lessor let go,
    /// and whatever is made from now given back at once.
    pub(crate) fn end(&self) {
        let runs = self.held.lock().ok().map(|mut held| {
            held.ended = true;
            std::mem::take(&mut held.runs)
        });
        for Run { lessor, leases } in runs.into_iter().flat_map(BTreeMap::into_values) {
            for lease in &leases {
                lessor.release(lease);
            }
        }
    }
}

/// No lessor of run `run` is held on this connection: its sign-in can't be answered here.
fn no_lessor(run: u64) -> SignInRefused {
    SignInRefused::Unavailable(format!("no lessor of run {run} is held on this connection"))
}
