//! Authority on the gdnet side (docs/PLAN_NETCODE.md 2.6, 3.1, WP7; the rules are in
//! netcore/src/authority.rs). The manager (manager.rs, "WP7" blocks) applies owner changes, answers
//! ownership queries, restores rejected writes and records the state of `client_owned` entities this
//! peer owns; this module holds the calls into user code it makes (always under the manager's base
//! guard), and the `NetCall` plumbing of `Net.request_owner`.

use crate::call::NetCall;
use crate::entity_node::NetEntity;
use godot::prelude::*;

/// Calls `_net_owner_changed(old, new)` on every part that defines it, then emits the `NetEntity`'s
/// `owner_changed(old, new)`. The caller holds its base guard (user code runs).
pub fn notify_owner_changed(parts: &[Gd<Node>], ne: Option<Gd<NetEntity>>, old: u32, new: u32) {
    let args = [i64::from(old).to_variant(), i64::from(new).to_variant()];
    for p in parts {
        if !p.is_instance_valid() || p.clone().try_cast::<NetEntity>().is_ok() {
            continue;
        }
        if p.has_method("_net_owner_changed") {
            let mut p = p.clone();
            p.call("_net_owner_changed", &args);
        }
    }
    if let Some(mut ne) = ne {
        if ne.has_method("_net_owner_changed") {
            ne.call("_net_owner_changed", &args);
        }
        ne.emit_signal("owner_changed", &args);
    }
}

/// The owner's answer to an ownership request of `requester`: every part that defines
/// `_net_grant_owner(requester) -> bool` must say yes; when none defines it, `default` (true for an
/// entity the server owns, false for one a client owns). The caller holds its base guard.
pub fn grant_owner(parts: &[Gd<Node>], requester: u32, default: bool) -> bool {
    let mut asked = false;
    let mut grant = true;
    for p in parts {
        if !p.is_instance_valid() || !p.has_method("_net_grant_owner") {
            continue;
        }
        asked = true;
        let mut p = p.clone();
        let yes = p
            .call("_net_grant_owner", &[i64::from(requester).to_variant()])
            .try_to::<bool>()
            .unwrap_or(false);
        grant &= yes;
    }
    if asked {
        grant
    } else {
        default
    }
}

/// A `NetCall` already resolved with `granted` (no session, or already the owner).
pub fn resolved(granted: bool) -> Gd<NetCall> {
    let call = NetCall::fresh();
    NetCall::resolve(&call, Ok(granted.to_variant()));
    call
}

/// A `NetCall` that failed with `reason` (printed as an error by `NetCall::failed`).
pub fn failed(reason: String) -> Gd<NetCall> {
    NetCall::failed(reason)
}
