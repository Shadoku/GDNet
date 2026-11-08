//! The post-tick hook (docs/PLAN_NETCODE.md 2.2 steps 5 and 6, WP3): a `NetPoller` node per session, an
//! internal child of the `NetSession`, at physics process priority +1000, so it runs after the game's
//! `_physics_process`. Each tick it calls `NetSession::post_tick()`: record (poll the managers' entities)
//! then send (snapshots, acks, and every packet netcore queued this tick).

use crate::session::NetSession;
use godot::classes::{INode, Node};
use godot::prelude::*;

/// Name of the poller under its session.
pub const POLLER_NODE: &str = "__NetPoller";

#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct NetPoller {
    base: Base<Node>,
}

#[godot_api]
impl INode for NetPoller {
    fn ready(&mut self) {
        self.base_mut().set_physics_process_priority(1000);
        self.base_mut().set_process_priority(1000);
    }

    fn physics_process(&mut self, _delta: f64) {
        let Some(parent) = self.base().get_parent() else {
            return;
        };
        if let Ok(mut s) = parent.try_cast::<NetSession>() {
            let _guard = self.base_mut();
            s.bind_mut().post_tick();
        }
    }
}
