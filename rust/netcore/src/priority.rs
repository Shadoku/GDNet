//! Priority accumulator and byte budget (docs/PLAN_NETCODE.md 2.7, WP3).
//!
//! Every tick, each entity with something to send to a peer adds its weight to that peer's accumulator
//! for it: [`OWNED_WEIGHT`] when the peer owns the entity, [`INTERACTING_WEIGHT`] when it is marked as
//! interacting with the peer (the hook WP14 scores), [`BASE_WEIGHT`] otherwise, times the entity's
//! `priority`. Snapshots take entities in accumulator order; an entity that was sent drops to zero, one
//! that did not fit keeps its sum and rises. Under a tight budget an entity of priority 10 is therefore
//! sent about twice as often as one of priority 5, and an owned entity every time.
//!
//! [`Budget`] is a token bucket per peer: `bytes_per_s / tick_rate` tokens a tick, capped at two MTUs,
//! so the average stays within the budget while a full-MTU snapshot is still possible every few ticks.
//! Interest management (rooms, radius, filters) is WP7/WP13.

use crate::entity::NetId;

/// The weight of an entity the receiving peer owns.
pub const OWNED_WEIGHT: f64 = 1_000_000.0;
/// The weight of an entity interacting with the receiving peer (set through the interacting hook).
pub const INTERACTING_WEIGHT: f64 = 100.0;
/// Every other entity.
pub const BASE_WEIGHT: f64 = 1.0;

/// The largest snapshot, in bytes (2.3: never fragmented).
pub const MTU: usize = 1200;
/// Snapshots smaller than this are not worth a packet: wait for more tokens.
pub const MIN_SNAPSHOT: usize = 48;

/// The weight an entity adds per tick.
pub fn weight(owned: bool, interacting: bool, priority: f32) -> f64 {
    let base = if owned {
        OWNED_WEIGHT
    } else if interacting {
        INTERACTING_WEIGHT
    } else {
        BASE_WEIGHT
    };
    base * f64::from(priority.max(0.0))
}

/// Sorts candidates `(id, accumulated)` for sending: highest sum first, then lowest id.
pub fn order(candidates: &mut [(NetId, f64)]) {
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
}

/// A per-peer token bucket of bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    tokens: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self { tokens: MTU as f64 }
    }
}

impl Budget {
    /// Adds one tick's worth of tokens.
    pub fn tick(&mut self, bytes_per_s: u32, tick_rate: u32) {
        let per_tick = f64::from(bytes_per_s) / f64::from(tick_rate.max(1));
        let cap = (2 * MTU) as f64 + per_tick;
        self.tokens = (self.tokens + per_tick).min(cap);
    }

    /// The largest snapshot this tick may send (0 when below [`MIN_SNAPSHOT`]).
    pub fn allowance(&self) -> usize {
        let t = self.tokens.max(0.0) as usize;
        let a = t.min(MTU);
        if a < MIN_SNAPSHOT {
            0
        } else {
            a
        }
    }

    pub fn spend(&mut self, bytes: usize) {
        self.tokens -= bytes as f64;
    }

    pub fn tokens(&self) -> f64 {
        self.tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights() {
        assert_eq!(weight(true, false, 1.0), OWNED_WEIGHT);
        assert_eq!(weight(false, true, 2.0), 200.0);
        assert_eq!(weight(false, false, 10.0), 10.0);
        assert_eq!(weight(false, false, -3.0), 0.0);
    }

    #[test]
    fn order_is_by_sum_then_id() {
        let mut c = [(3, 1.0), (1, 5.0), (2, 5.0), (4, 0.5)];
        order(&mut c);
        assert_eq!(c.map(|x| x.0), [1, 2, 3, 4]);
    }

    #[test]
    fn budget_averages_to_its_rate() {
        let mut b = Budget::default();
        let mut sent = 0usize;
        for _ in 0..600 {
            b.tick(32_768, 60);
            let a = b.allowance();
            if a > 0 {
                b.spend(a);
                sent += a;
            }
        }
        // Ten seconds: within the budget plus the initial bucket.
        assert!(sent <= 327_680 + 3 * MTU, "{sent}");
        assert!(sent >= 300_000, "{sent}");
        let mut tiny = Budget::default();
        tiny.spend(MTU);
        tiny.tick(60, 60);
        assert_eq!(tiny.allowance(), 0, "below the minimum snapshot");
    }
}
