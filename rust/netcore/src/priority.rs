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
//!
//! WP14: [`near`] is the proximity half of interaction scoring (2.11): given each peer's player
//! position and every entity's position and radius, it lists the entities within that radius of the
//! player, through a coarse grid of cells as large as the largest radius (each player looks at the
//! 3x3x3 cells around it). gdnet reads the positions (engine data), adds the bodies in contact with
//! the player, and feeds the result to `Session::set_interacting` every server tick before the
//! snapshots are built.

use crate::entity::NetId;
use crate::peers::PeerId;
use std::collections::BTreeMap;
use std::vec::Vec;

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
    candidates.sort_by(|a, b| cmp(a.0, a.1, b.0, b.1));
}

/// The sending order of two candidates (id, accumulated sum): highest sum first, then lowest id.
pub fn cmp(a_id: NetId, a_sum: f64, b_id: NetId, b_sum: f64) -> core::cmp::Ordering {
    b_sum.total_cmp(&a_sum).then(a_id.cmp(&b_id))
}

/// WP14: an entity's position and the distance within which it counts as near a player.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Body {
    pub id: NetId,
    pub pos: [f64; 3],
    /// 0 or less: never near by distance.
    pub radius: f64,
}

/// WP14: for each player `(peer, player entity, position)`, the ids of the bodies within their
/// radius of the player (the player itself left out), ascending. A coarse grid keeps it near linear
/// in the number of bodies.
pub fn near(players: &[(PeerId, NetId, [f64; 3])], bodies: &[Body]) -> Vec<(PeerId, Vec<NetId>)> {
    let cell = bodies.iter().map(|b| b.radius).fold(0.0f64, f64::max);
    if cell <= 0.0 || !cell.is_finite() {
        return players.iter().map(|p| (p.0, Vec::new())).collect();
    }
    let key = |p: &[f64; 3]| {
        (
            (p[0] / cell).floor() as i64,
            (p[1] / cell).floor() as i64,
            (p[2] / cell).floor() as i64,
        )
    };
    let mut grid: BTreeMap<(i64, i64, i64), Vec<usize>> = BTreeMap::new();
    for (i, b) in bodies.iter().enumerate() {
        if b.radius > 0.0 && b.pos.iter().all(|v| v.is_finite()) {
            grid.entry(key(&b.pos)).or_default().push(i);
        }
    }
    let mut out = Vec::with_capacity(players.len());
    for &(peer, player, pos) in players {
        let (cx, cy, cz) = key(&pos);
        let mut ids = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    let Some(list) = grid.get(&(cx + dx, cy + dy, cz + dz)) else {
                        continue;
                    };
                    for &i in list {
                        let b = &bodies[i];
                        if b.id == player {
                            continue;
                        }
                        let d2: f64 = (0..3).map(|k| (b.pos[k] - pos[k]).powi(2)).sum();
                        if d2 <= b.radius * b.radius {
                            ids.push(b.id);
                        }
                    }
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        out.push((peer, ids));
    }
    out
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
    fn near_lists_bodies_within_their_radius() {
        let mut bodies = Vec::new();
        for i in 0..300u32 {
            let x = f64::from(i % 30) * 20.0;
            let y = f64::from(i / 30) * 20.0;
            bodies.push(Body {
                id: i + 1,
                pos: [x, y, 0.0],
                radius: 50.0,
            });
        }
        // A body with a smaller radius of its own, and the player's own entity.
        bodies.push(Body {
            id: 1000,
            pos: [130.0, 100.0, 0.0],
            radius: 10.0,
        });
        bodies.push(Body {
            id: 2000,
            pos: [100.0, 100.0, 0.0],
            radius: 50.0,
        });
        let players = [(5, 2000, [100.0, 100.0, 0.0]), (6, 0, [1e6, 0.0, 0.0])];
        let out = near(&players, &bodies);
        assert_eq!(out.len(), 2);
        let (peer, ids) = &out[0];
        assert_eq!(*peer, 5);
        let brute: Vec<NetId> = bodies
            .iter()
            .filter(|b| {
                b.id != 2000
                    && ((b.pos[0] - 100.0).powi(2) + (b.pos[1] - 100.0).powi(2)).sqrt() <= b.radius
            })
            .map(|b| b.id)
            .collect();
        let mut brute = brute;
        brute.sort_unstable();
        assert_eq!(ids, &brute, "the grid finds what a linear pass finds");
        assert!(!ids.contains(&1000), "30 px away with a 10 px radius");
        assert!(!ids.contains(&2000), "never the player itself");
        assert!(ids.len() > 10 && ids.len() < 40, "{}", ids.len());
        assert!(out[1].1.is_empty(), "a far player is near nothing");
        assert!(near(&players, &[]).iter().all(|p| p.1.is_empty()));
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
