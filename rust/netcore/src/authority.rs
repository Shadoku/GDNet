//! Authority (docs/PLAN_NETCODE.md 2.6, WP7): who owns an entity, how ownership moves, and whose state
//! is accepted.
//!
//! Every entity has one owner peer (1 = the server by default), replicated on change with
//! `EntityAction::SetOwner`. How it may move is the entity's [`Transfer`] rule, carried by its `Create`:
//!
//! | rule | a client's `RequestOwner` |
//! |---|---|
//! | `fixed` (default) | refused |
//! | `request` | the current owner decides: the server asks gdnet (`_net_grant_owner(requester)` on the parts; default yes for a server-owned entity) or, for a client-owned entity, sends `OwnerQuery` to that client, whose gdnet answers (default no) |
//! | `take` | granted |
//!
//! A grant is one `SetOwner` broadcast; the requester then gets `OwnerReply { granted }`. `ReleaseOwner`
//! from the owner gives the entity back to the server. Only the server changes owners.
//!
//! Acceptance: input only from the owner (WP4); state only from the server, except entities flagged
//! `client_owned`, whose owner sends its state ([`StateMsg`], kind `OWNER_STATE`, unreliable, every tick
//! something changed and for a few ticks after, plus a keyframe every second). The server applies it to
//! its record, hands it to gdnet for its node and relays it in its snapshots to every other peer; the
//! owner never gets its own state back. A client's change to an entity it does not own (detected by
//! comparing a replica's polled values with the last values the server sent, [`write_checked`] fields
//! only) travels as a reliable `WRITE` and is answered with a [`WriteReject`] naming the fields; the
//! writer then restores the server's values (Normcore's rollback of rejected writes).

use crate::entity::NetId;
use crate::props::PropDesc;
use serde::{Deserialize, Serialize};
use std::vec::Vec;

/// `NetEntity.transfer`: how ownership of an entity may move (2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Transfer {
    /// Only the server moves it (`Net.set_owner`).
    #[default]
    Fixed,
    /// A client may ask; the current owner decides.
    Request,
    /// A client may take it at any time.
    Take,
}

impl Transfer {
    pub fn from_index(i: i64) -> Self {
        match i {
            1 => Transfer::Request,
            2 => Transfer::Take,
            _ => Transfer::Fixed,
        }
    }
}

/// What the server does with a request for ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Grant,
    Refuse,
    /// The current owner (this peer id) must decide.
    Ask(crate::peers::PeerId),
}

/// The rule of [`Transfer`] for a request by `requester` of an entity owned by `owner`.
pub fn decide(
    transfer: Transfer,
    owner: crate::peers::PeerId,
    requester: crate::peers::PeerId,
) -> Decision {
    if owner == requester {
        return Decision::Grant;
    }
    match transfer {
        Transfer::Fixed => Decision::Refuse,
        Transfer::Take => Decision::Grant,
        Transfer::Request => Decision::Ask(owner),
    }
}

/// The body of `OWNER_STATE` (unreliable) and `WRITE` (reliable): per entity a header-less snapshot
/// entry (see `Replication::encode_values`) with the fields the sender claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StateMsg {
    /// The sender's tick (the server drops an owner state older than the newest it applied).
    pub tick: u64,
    pub entities: Vec<(NetId, Vec<u8>)>,
}

/// `WRITE_REJECT`: the server refused the writer's change of these fields of `id` (`(part, field)`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReject {
    pub id: NetId,
    pub fields: Vec<(u16, u16)>,
}

/// Owner states repeat for this many ticks after the last change (loss cover), then stop until the
/// next change or the keyframe.
pub const OWNER_STATE_REDUNDANCY: u64 = 3;

/// An owner state packet is cut before it grows beyond this many bytes.
pub const OWNER_STATE_MAX_BYTES: usize = 1100;

/// Properties the engine changes by itself on every peer (animation playback, particles running out):
/// a replica's own value drifts from the server's without any code writing it, so they are never
/// treated as writes.
pub const LOCALLY_DRIVEN: [&str; 6] = [
    "frame",
    "frame_progress",
    "playing",
    "current_animation",
    "current_animation_position",
    "emitting",
];

/// Whether a replica's local change of this property counts as a write (and is rejected): not an
/// interpolated field (the client writes those itself every frame), not a slow field, not a property
/// the engine drives locally.
pub fn write_checked(d: &PropDesc) -> bool {
    !d.interpolate && d.slow_ticks == 0 && !LOCALLY_DRIVEN.contains(&d.name.as_str())
}

/// Counters for diagnostics (`NetSession.stats()`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorityStats {
    /// Owner changes this side applied (server: made; client: received).
    pub owner_changes: u64,
    /// Server: rejects sent; client: rejects received.
    pub writes_rejected: u64,
    /// Client: writes sent.
    pub writes_sent: u64,
    pub owner_states_sent: u64,
    pub owner_states_received: u64,
    /// Ownership requests refused (server: refused; client: refusals received).
    pub requests_refused: u64,
    pub requests_granted: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::TypeTag;

    #[test]
    fn transfer_rules() {
        assert_eq!(decide(Transfer::Fixed, 1, 5), Decision::Refuse);
        assert_eq!(decide(Transfer::Take, 1, 5), Decision::Grant);
        assert_eq!(decide(Transfer::Take, 6, 5), Decision::Grant);
        assert_eq!(decide(Transfer::Request, 1, 5), Decision::Ask(1));
        assert_eq!(decide(Transfer::Request, 6, 5), Decision::Ask(6));
        assert_eq!(
            decide(Transfer::Fixed, 5, 5),
            Decision::Grant,
            "already the owner"
        );
        assert_eq!(Transfer::from_index(2), Transfer::Take);
        assert_eq!(Transfer::from_index(9), Transfer::Fixed);
    }

    #[test]
    fn writes_skip_interpolated_slow_and_engine_driven_fields() {
        let mut d = PropDesc::new("z_index", TypeTag::Int);
        assert!(write_checked(&d));
        d.interpolate = true;
        assert!(!write_checked(&d));
        d.interpolate = false;
        d.slow_ticks = 60;
        assert!(!write_checked(&d));
        assert!(!write_checked(&PropDesc::new("frame", TypeTag::Int)));
    }
}
