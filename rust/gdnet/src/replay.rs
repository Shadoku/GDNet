//! Rollback and replay (docs/PLAN_NETCODE.md 2.2 step 3, 2.5, WP4).
//!
//! When netcore reports that a snapshot of server tick T disagrees with what the owner predicted for T
//! (a [`Rollback`]), the owner restores the hot properties of every entity it predicts to their state at
//! T (the server's confirmed values, `apply::write`), then for each tick T+1 up to the last tick it
//! simulated: sets each entity's `NetInput` to the frame it used for that tick, calls `_physics_process`
//! (or `_net_tick` with `is_fresh` false) directly on the entity's parts in tree order, and records the
//! new hot state into the history. `Net.is_fresh()` is false for the whole replay. Then the entities'
//! `NetEntity.rolled_back(from_tick, to_tick)` and the session's `rolled_back(node, from_tick, to_tick)`
//! fire, and the fresh tick runs as usual.
//!
//! Catch-up: when the client's prediction lead grows, its predicted tick jumps; the ticks it skipped are
//! simulated at once the same way (with the frames stored for them, `is_fresh` true: they are new
//! ticks), so the owner stays in step with what the server simulates.
//!
//! WP5, blending instead of snapping: before the restore the interpolated hot fields of the entities
//! are read and combined with their owned offsets (what the player saw); after the replay the
//! difference to the replayed values becomes each field's owned offset, shown in `_process` and
//! decaying (interp.rs). The properties themselves hold the exact replayed values.
//!
//! Known limit (inherited from the engine, shared with netfox): physics-server queries such as
//! `is_on_floor()` are not re-run with the restored state until `move_and_slide()` updates them, so they
//! are stale during a replay; derive such values from state (or declare them as state).

use crate::manager::NetManager;
use crate::session::NetSession;
use crate::tick;
use godot::prelude::*;
use netcore::{NetId, Rollback};

/// Restores and replays; returns the number of ticks replayed (0 when no entity of `rb` was found).
/// `now` is the predicted tick about to be simulated fresh (the replay ends before it).
pub fn rollback(
    session: &Gd<NetSession>,
    managers: &[(u64, Gd<NetManager>)],
    rb: &Rollback,
    now: u64,
    delta: f64,
) -> u64 {
    let ids: Vec<NetId> = rb.entities.iter().map(|(id, _)| *id).collect();
    // WP5: what was shown before the rollback (the simulated value plus any owned offset).
    let mut before = Vec::new();
    for (_, m) in managers {
        let mut m = m.clone();
        before.extend(m.bind_mut().interp_hot(&ids));
    }
    let mut s = session.clone();
    let shown_before: Vec<_> = {
        let r = s.bind_mut().with_core(|c, _| {
            before
                .into_iter()
                .map(|(k, sim)| {
                    let v = c.apply_owned_offset(k.0, k.1, k.2, &sim).unwrap_or(sim);
                    (k, v)
                })
                .collect()
        });
        r.unwrap_or_default()
    };
    let mut restored: Vec<(Gd<NetManager>, Vec<NetId>)> = Vec::new();
    for (_, m) in managers {
        let mut m = m.clone();
        let ids = m.bind_mut().restore(&rb.entities);
        if !ids.is_empty() {
            restored.push((m, ids));
        }
    }
    if restored.is_empty() {
        return 0;
    }
    let mut replayed = 0;
    {
        let _replaying = tick::Replaying::start();
        for t in rb.to_tick + 1..now {
            for (m, ids) in &restored {
                let mut m = m.clone();
                let hist = m.bind_mut().replay_tick(session, ids, t, delta);
                let mut s = session.clone();
                s.bind_mut().with_core(|c, _| {
                    for (id, values) in hist {
                        c.record_prediction(id, t, values);
                    }
                });
            }
            replayed += 1;
        }
    }
    // WP5: the difference between what was shown and the replayed state blends out visually.
    let mut after = Vec::new();
    for (m, ids) in &restored {
        let mut m = m.clone();
        after.extend(m.bind_mut().interp_hot(ids));
    }
    {
        let mut s = session.clone();
        s.bind_mut().with_core(|c, _| {
            for (k, replayed) in &after {
                if let Some((_, shown)) = shown_before.iter().find(|(kb, _)| kb == k) {
                    c.blend_owned(k.0, k.1, k.2, shown, replayed);
                }
            }
        });
    }
    let from = now.saturating_sub(1).max(rb.to_tick);
    for (m, ids) in &restored {
        let mut m = m.clone();
        m.bind_mut()
            .emit_rolled_back(session, ids, from, rb.to_tick);
    }
    replayed
}

/// Simulates the predicted ticks `from..now` of every predicted entity (the lead grew).
pub fn catch_up(
    session: &Gd<NetSession>,
    managers: &[(u64, Gd<NetManager>)],
    from: u64,
    now: u64,
    delta: f64,
) {
    for (_, m) in managers {
        let mut m = m.clone();
        let ids = m.bind().predicted_ids();
        if ids.is_empty() {
            continue;
        }
        for t in from..now {
            let hist = m.bind_mut().replay_tick(session, &ids, t, delta);
            let mut s = session.clone();
            s.bind_mut().with_core(|c, _| {
                for (id, values) in hist {
                    c.record_prediction(id, t, values);
                }
            });
        }
    }
}
