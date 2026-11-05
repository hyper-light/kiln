//! The open's ticket (docs/browser-protocol.md, "The ticket"): 32 random bytes the server mints
//! for a fetch that carries the browser's session cookie, good for one open, once, within its
//! life. WebTransport sends no cookies, so the protocol authenticates inside itself the same way
//! on every carrier. The randomness and the clock are the caller's.

use std::collections::BTreeMap;

use crate::frames::OpenRefusal;

/// A ticket's life.
pub const TICKET_LIFE_MS: u64 = 30_000;

/// Who a connection is for: the person (as the audit log names them) and the browser session
/// its ticket was minted from, with when that session expires.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Principal {
    pub who: String,
    pub session: String,
    /// When the browser session ends, on the caller's clock; the connection is cut then.
    pub expires_at: Option<u64>,
}

/// What redeems a ticket at open: kiln-bp's [`Tickets`], or the server's own.
pub trait Redeem {
    fn redeem(&mut self, ticket: &[u8], now: u64) -> Result<Principal, OpenRefusal>;
}

#[derive(Debug, Clone)]
struct Minted {
    principal: Principal,
    at: u64,
    taken: bool,
}

/// The tickets a server minted and hasn't yet let go of.
#[derive(Debug, Clone, Default)]
pub struct Tickets {
    minted: BTreeMap<Vec<u8>, Minted>,
    life_ms: u64,
}

impl Tickets {
    pub fn new() -> Self {
        Self { minted: BTreeMap::new(), life_ms: TICKET_LIFE_MS }
    }

    pub fn with_life(life_ms: u64) -> Self {
        Self { minted: BTreeMap::new(), life_ms }
    }

    /// Mints a ticket of the caller's random bytes for `principal`.
    pub fn mint(&mut self, random: [u8; 32], principal: Principal, now: u64) -> Vec<u8> {
        self.forget(now);
        let t = random.to_vec();
        self.minted.insert(t.clone(), Minted { principal, at: now, taken: false });
        t
    }

    /// Lets go of tickets past twice their life: one presented after that reads as never made.
    fn forget(&mut self, now: u64) {
        let keep = self.life_ms.saturating_mul(2);
        self.minted.retain(|_, m| now.saturating_sub(m.at) <= keep);
    }

    pub fn held(&self) -> usize {
        self.minted.len()
    }
}

impl Redeem for Tickets {
    fn redeem(&mut self, ticket: &[u8], now: u64) -> Result<Principal, OpenRefusal> {
        self.forget(now);
        let life = self.life_ms;
        let m = self.minted.get_mut(ticket).ok_or(OpenRefusal::BadTicket)?;
        if m.taken {
            return Err(OpenRefusal::Reused);
        }
        if now.saturating_sub(m.at) > life {
            return Err(OpenRefusal::Expired);
        }
        m.taken = true;
        Ok(m.principal.clone())
    }
}
