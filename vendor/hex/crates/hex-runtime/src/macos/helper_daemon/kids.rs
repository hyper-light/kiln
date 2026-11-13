//! The programs the helper started, which only it can reap: it is their parent, and `waitid` answers no
//! other caller (`bsd/kern/kern_exit.c:3194-3361`, `ECHILD`). Each is watched from its start
//! ([`super::Privileged::watch`]), reaped by the daemon's sweep or by a node's [`Request::Wait`]
//! (`hex_core::helper`), and its end held until the node reads it once.
//!
//! Bounds (CLAUDE.md §2): a box holds at most its `cap` programs (its process cap, given when it is
//! opened), live or ended and unread. A start past
//! that drops the box's oldest unread end first (a later wait on it reads [`Exit::Unknown`], as an end
//! no one collected), and is refused when all `cap` still run. An ended box's unread ends are kept for
//! the node's waits on them (a delete's programs end by its kill), at most `graves` boxes' worth, the
//! oldest ended box dropped first; acquiring its id again drops them too.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Exit, Proc};

/// One box's programs.
struct Started {
    /// The most programs it holds: its process cap.
    cap: usize,
    live: BTreeMap<i32, Box<dyn Proc>>,
    /// Ended programs whose end the node hasn't read, oldest first.
    ended: VecDeque<(i32, Exit)>,
}

impl Started {
    fn held(&self) -> usize {
        self.live.len().saturating_add(self.ended.len())
    }

    /// Reaps each live program that has ended (never blocks on one still running).
    fn sweep(&mut self) {
        let mut done = Vec::new();
        for (&pid, p) in &mut self.live {
            if let Ok(Some(exit)) = p.try_wait() {
                done.push((pid, exit));
            }
        }
        for (pid, exit) in done {
            self.live.remove(&pid);
            self.ended.push_back((pid, exit));
        }
    }
}

/// A program taken out to be waited on.
pub enum Taken {
    /// Ended, its end read now.
    Ended(Exit),
    /// Still running when taken; [`Kids::put_back`] returns it if it still runs after the wait.
    Live(Box<dyn Proc>),
}

/// Why a program can't be held.
#[derive(Debug, PartialEq, Eq)]
pub enum Full {
    /// The box has no entry (never acquired, or dropped).
    NoBox,
    /// All `cap` of the box's programs still run.
    Running(usize),
}

/// Every box's programs, live boxes and recently ended ones.
pub struct Kids {
    boxes: BTreeMap<BoxId, Started>,
    /// Ended boxes, oldest first; their unread ends stay until a wait reads them or they are dropped.
    graves: VecDeque<BoxId>,
    graves_max: usize,
}

impl Kids {
    /// At most `graves_max` ended boxes' unread ends; each box's own cap is given when it is opened.
    pub fn new(graves_max: usize) -> Self {
        Self {
            boxes: BTreeMap::new(),
            graves: VecDeque::new(),
            graves_max,
        }
    }

    /// A box acquired, holding at most `cap` programs: a fresh entry (an ended box of the same id is
    /// dropped).
    pub fn open(&mut self, id: &BoxId, cap: usize) {
        self.graves.retain(|g| g != id);
        self.boxes.insert(
            id.clone(),
            Started {
                cap,
                live: BTreeMap::new(),
                ended: VecDeque::new(),
            },
        );
    }

    /// Room for one more program in box `id`: drops its oldest unread end when it holds `cap`; refused
    /// when all `cap` still run.
    pub fn room(&mut self, id: &BoxId) -> Result<(), Full> {
        let b = self.boxes.get_mut(id).ok_or(Full::NoBox)?;
        let cap = b.cap;
        b.sweep();
        while b.held() >= cap {
            if b.ended.pop_front().is_none() {
                return Err(Full::Running(cap));
            }
        }
        Ok(())
    }

    /// Holds program `pid`, started in box `id`, watched by `proc`.
    pub fn hold(&mut self, id: &BoxId, pid: i32, proc: Box<dyn Proc>) -> Result<(), Full> {
        self.room(id)?;
        let b = self.boxes.get_mut(id).ok_or(Full::NoBox)?;
        b.live.insert(pid, proc);
        Ok(())
    }

    /// Takes program `pid` of box `id` to read its end: `None` when the box has no entry, or the
    /// program isn't one it holds (never started there, or its end read already).
    pub fn take(&mut self, id: &BoxId, pid: i32) -> Option<Option<Taken>> {
        let b = self.boxes.get_mut(id)?;
        if let Some(at) = b.ended.iter().position(|&(p, _)| p == pid) {
            return Some(b.ended.remove(at).map(|(_, exit)| Taken::Ended(exit)));
        }
        Some(b.live.remove(&pid).map(Taken::Live))
    }

    /// Kills program `pid` of box `id` where the box holds it running (`Proc::kill`), leaving it held for its
    /// end to be read: `None` when the box has no entry; a program the box doesn't hold running is sent
    /// nothing.
    pub fn kill(&mut self, id: &BoxId, pid: i32) -> Option<Result<(), hex_api::Refusal>> {
        let b = self.boxes.get_mut(id)?;
        Some(b.live.get_mut(&pid).map_or(Ok(()), |p| p.kill()))
    }

    /// Returns a program still running after a wait; `Err` hands it back when its box was dropped
    /// meanwhile, for the caller to reap.
    pub fn put_back(
        &mut self,
        id: &BoxId,
        pid: i32,
        proc: Box<dyn Proc>,
    ) -> Result<(), Box<dyn Proc>> {
        match self.boxes.get_mut(id) {
            Some(b) => {
                b.live.insert(pid, proc);
                Ok(())
            }
            None => Err(proc),
        }
    }

    /// Reaps every box's ended programs (the daemon's sweep), so none stays a zombie under its user.
    pub fn sweep(&mut self) {
        for b in self.boxes.values_mut() {
            b.sweep();
        }
    }

    /// Box `id` ended (its user killed and empty): reaps each of its programs, waiting at most until
    /// `deadline` for each zombie, and keeps their ends as a grave. Returns how many it couldn't reap.
    pub fn close(&mut self, id: &BoxId, deadline: Option<Instant>) -> usize {
        let Some(b) = self.boxes.get_mut(id) else {
            return 0;
        };
        let live = std::mem::take(&mut b.live);
        let mut unreaped = 0usize;
        for (pid, mut p) in live {
            let left = deadline.map_or(Duration::ZERO, |d| {
                d.saturating_duration_since(Instant::now())
            });
            match p.wait_for(left) {
                Ok(Some(exit)) => b.ended.push_back((pid, exit)),
                _ => {
                    unreaped = unreaped.saturating_add(1);
                    b.ended.push_back((pid, Exit::Unknown));
                }
            }
        }
        self.graves.retain(|g| g != id);
        self.graves.push_back(id.clone());
        while self.graves.len() > self.graves_max {
            if let Some(old) = self.graves.pop_front() {
                self.boxes.remove(&old);
            }
        }
        unreaped
    }
}
