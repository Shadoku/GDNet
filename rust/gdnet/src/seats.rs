//! WP17: seats, rejoining and session state on the Godot side (docs/PLAN_NETCODE.md 12.3; the rules
//! are netcore's `seats` and `savestate`).
//!
//! - Keys: every `NetSession` has a seat key (`NetSession.key`), a random string per session object
//!   unless the game sets one before `start()`. `Net` keeps one random key for the life of the process
//!   (`Net.key`), which its sessions use; `Net.join(address, port, key)` passes another.
//! - The rejoin window: `NetSession.rejoin_timeout_s`, else `Net.rejoin_timeout_s`, else the project
//!   setting `gdnet/session/rejoin_timeout_s` (60; 0 = released at once; -1 = the whole session),
//!   handed to netcore every tick.
//! - A rejoin moves netcore's per-peer state itself; here the server moves what gdnet holds per peer:
//!   the focus node (`set_focus`) and the `visible_to` peer lists stored on the entity roots.
//! - `save_state()`: netcore's keyed state and seats plus every level's state (loaded levels are
//!   captured now; unloaded ones are their kept blobs). A session started from one restores the keyed
//!   state, reserves the seats and keeps the level blobs for when those levels load; a restored entity
//!   whose blob names an owner seat goes to that seat's peer once it is connected.

use crate::interest::{store_visible_to, visible_of};
use crate::net::Net;
use crate::session::NetSession;
use godot::classes::RandomNumberGenerator;
use godot::prelude::*;
use netcore::savestate::SessionState;
use netcore::Visible;
use std::cell::{Cell, RefCell};

/// The project setting for the rejoin window (seconds).
pub const SETTING_REJOIN: &str = "gdnet/session/rejoin_timeout_s";
/// The rejoin window without a setting (D22).
pub const DEFAULT_REJOIN_S: f64 = netcore::session::DEFAULT_REJOIN_TIMEOUT_S;

thread_local! {
    /// `Net.rejoin_timeout_s` (NaN = the project setting).
    static NET_REJOIN: Cell<f64> = const { Cell::new(f64::NAN) };
    /// `Net.key` (empty until first read: then a random key for the life of the process).
    static NET_KEY: RefCell<String> = const { RefCell::new(String::new()) };
}

/// A random seat key (128 bits as hex).
pub fn random_key() -> String {
    let mut rng = RandomNumberGenerator::new_gd();
    rng.randomize();
    format!(
        "{:08x}{:08x}{:08x}{:08x}",
        rng.randi(),
        rng.randi(),
        rng.randi(),
        rng.randi()
    )
}

/// `gdnet/session/rejoin_timeout_s`, else 60.
pub fn rejoin_setting() -> f64 {
    crate::hash::setting(SETTING_REJOIN)
        .and_then(|v| crate::profiles::number(&v))
        .unwrap_or(DEFAULT_REJOIN_S)
}

/// `Net.rejoin_timeout_s` when set, else the project setting.
pub fn net_rejoin() -> f64 {
    let v = NET_REJOIN.with(Cell::get);
    if v.is_nan() {
        rejoin_setting()
    } else {
        v
    }
}

/// A written rejoin window: below -1 = back to the default (NaN), anything else as given (a negative
/// value above -1 counts as -1, the whole session).
pub fn normalise(v: f64) -> f64 {
    if v.is_nan() || v < -1.0 {
        f64::NAN
    } else if v < 0.0 {
        -1.0
    } else {
        v
    }
}

/// `Net.key`: the process's seat key.
pub fn net_key() -> String {
    NET_KEY.with(|k| {
        let mut k = k.borrow_mut();
        if k.is_empty() {
            *k = random_key();
        }
        k.clone()
    })
}

/// `Net.key = key` ("" picks a new random key).
pub fn set_net_key(key: &str) {
    let k = if key.is_empty() {
        random_key()
    } else {
        key.to_string()
    };
    NET_KEY.with(|c| *c.borrow_mut() = k);
}

/// `Net.rejoin_timeout_s = v`.
pub fn set_net_rejoin(v: f64) {
    NET_REJOIN.with(|c| c.set(normalise(v)));
}

/// An optional `state` argument: left out or null = no state; a `PackedByteArray` as given; anything
/// else is refused later as not a session state.
pub fn state_arg(arg: crate::call::NetArg) -> PackedByteArray {
    let v = arg.or_nil();
    if v.is_nil() {
        return PackedByteArray::new();
    }
    v.try_to::<PackedByteArray>()
        .unwrap_or_else(|_| PackedByteArray::from(&[0u8][..]))
}

/// Decodes a session state handed to `start()`: None for empty bytes or a joining client.
pub fn decode_state(
    bytes: &PackedByteArray,
    hosting: bool,
) -> Result<Option<SessionState>, String> {
    if bytes.is_empty() || !hosting {
        return Ok(None);
    }
    SessionState::decode(bytes.as_slice())
        .map(Some)
        .map_err(|e| e.to_string())
}

#[godot_api(secondary)]
impl NetSession {
    /// WP17: the seat of `peer` (also the last peer of a seat that is away); 0 without one.
    #[func]
    pub fn seat(&self, peer: i64) -> i64 {
        self.core
            .as_ref()
            .and_then(|c| c.seat(peer.max(0) as u32))
            .map_or(0, i64::from)
    }

    /// WP17: the peer connected on `seat`; 0 while it is away (or unknown).
    #[func]
    pub fn peer_of(&self, seat: i64) -> i64 {
        self.core
            .as_ref()
            .map_or(0, |c| i64::from(c.peer_of(seat.max(0) as u32)))
    }

    /// WP17: this side's seat (a client's from its `Welcome`; a listen server's); 0 without one.
    #[func]
    pub fn my_seat(&self) -> i64 {
        self.core
            .as_ref()
            .and_then(|c| c.my_seat())
            .map_or(0, i64::from)
    }

    /// WP17: every seat, ascending, as Dictionaries `{seat, peer, away}` (`peer` is the last holder
    /// while `away`).
    #[func]
    pub fn seat_table(&self) -> VarArray {
        let mut out = VarArray::new();
        if let Some(c) = self.core.as_ref() {
            for r in c.seat_table() {
                let mut d = VarDictionary::new();
                d.set("seat", i64::from(r.seat));
                d.set("peer", i64::from(r.peer));
                d.set("away", r.away);
                out.push(&d.to_variant());
            }
        }
        out
    }

    /// WP17, server: the session state as bytes (the keyed state, the seats and their keys, and every
    /// level's state), for the game to write where it likes; `start(peer, path, state)` /
    /// `Net.host(port, state)` / `Net.offline(state)` start from it. Empty (with an error) on a client
    /// or without a session.
    #[func]
    pub fn save_state(&mut self) -> PackedByteArray {
        if !self.is_server() {
            godot_error!("gdnet: save_state is for the server (or an offline session)");
            return PackedByteArray::new();
        }
        let Some(mut st) = self.core.as_ref().map(|c| c.session_state()) else {
            return PackedByteArray::new();
        };
        let mut levels: std::collections::BTreeMap<String, Vec<u8>> = self.kept_levels.clone();
        let loaded: Vec<(Gd<Node>, String)> = self
            .levels
            .iter()
            .filter(|l| l.node.is_instance_valid())
            .map(|l| (l.node.clone(), l.path.clone()))
            .collect();
        for (node, path) in loaded {
            if let Some(ls) = self.capture_level(&node, &path) {
                levels.insert(path, ls.encode());
            }
        }
        st.levels = levels.into_iter().collect();
        PackedByteArray::from(st.encode().as_slice())
    }
}

impl NetSession {
    /// Server: `new` took back the seat `old` held: the focus node and the `visible_to` lists on the
    /// entity roots follow (netcore already moved its own per-peer state).
    pub(crate) fn seat_rejoined(&mut self, old: u32, new: u32) {
        if !self.is_server() {
            return;
        }
        if let Some(f) = self.interest.focus.remove(&old) {
            self.interest.focus.insert(new, f);
        }
        let ids: Vec<netcore::NetId> = self
            .core
            .as_ref()
            .map(|c| {
                c.entities()
                    .iter()
                    .map(|r| r.id)
                    .filter(|id| {
                        matches!(c.interest().visible_of(*id), Visible::Peers(ref l) if l.contains(&new))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        let managers = self.managers();
        let _guard = self.base_mut();
        for id in ids {
            let Some(root) = managers.iter().find_map(|(_, m)| m.bind().root_of_id(id)) else {
                continue;
            };
            if let Visible::Peers(list) = visible_of(&root) {
                if list.contains(&old) {
                    let peers: PackedInt32Array = list
                        .iter()
                        .map(|p| if *p == old { new as i32 } else { *p as i32 })
                        .collect();
                    let _ = store_visible_to(&root, &peers.to_variant());
                }
            }
        }
    }

    /// Server, each step while claims wait: a restored entity whose owner seat is connected goes to
    /// that seat's peer; claims of freed nodes or released seats are dropped.
    pub(crate) fn process_seat_claims(&mut self) {
        if self.seat_claims.is_empty() {
            return;
        }
        let Some(core) = self.core.as_ref() else {
            return;
        };
        let table = core.seat_table();
        let mut ready = Vec::new();
        self.seat_claims.retain(|(seat, node)| {
            if !node.is_instance_valid() || !table.iter().any(|r| r.seat == *seat) {
                return false;
            }
            let peer = table
                .iter()
                .find(|r| r.seat == *seat && !r.away)
                .map_or(0, |r| r.peer);
            if peer == 0 {
                return true;
            }
            ready.push((node.clone(), peer));
            false
        });
        if ready.is_empty() {
            return;
        }
        let _guard = self.base_mut();
        for (node, peer) in ready {
            if let Some((root, mut m)) = crate::manager::entity_root_of(&node) {
                m.bind_mut().set_entity_owner(&root, peer);
            }
        }
    }
}

#[godot_api(secondary)]
impl Net {
    /// WP17: the seat of `peer` in `Net`'s session; 0 without one.
    #[func]
    fn seat(&self, peer: i64) -> i64 {
        self.default_session().map_or(0, |s| s.bind().seat(peer))
    }

    /// WP17: the peer on `seat` in `Net`'s session; 0 while away.
    #[func]
    fn peer_of(&self, seat: i64) -> i64 {
        self.default_session().map_or(0, |s| s.bind().peer_of(seat))
    }

    /// WP17: this process's seat in `Net`'s session; 0 without one.
    #[func]
    fn my_seat(&self) -> i64 {
        self.default_session().map_or(0, |s| s.bind().my_seat())
    }

    /// WP17: `Net`'s session's seat table (see `NetSession.seat_table`).
    #[func]
    fn seat_table(&self) -> VarArray {
        self.default_session()
            .map_or_else(VarArray::new, |s| s.bind().seat_table())
    }

    /// WP17: `Net`'s session's state as bytes (see `NetSession.save_state`).
    #[func]
    fn save_state(&self) -> PackedByteArray {
        match self.default_session() {
            Some(mut s) => s.bind_mut().save_state(),
            None => {
                godot_error!("gdnet: Net.save_state needs a session (host or offline)");
                PackedByteArray::new()
            }
        }
    }
}
