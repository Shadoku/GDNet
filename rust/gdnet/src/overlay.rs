//! WP8: `NetOverlay`, the in-game ownership overlay (docs/PLAN_NETCODE.md 1 "What the developer sees",
//! D9). A `CanvasLayer` that, each render frame, puts a small `Label` at the screen position of every
//! managed entity whose root is a `Node2D` (`get_global_transform_with_canvas()`) or a `Node3D` (the
//! viewport's current `Camera3D.unproject_position()`; skipped without a camera or behind it), reading
//! `#<net id> owner <peer>`, coloured by role, Roblox-style: green = owned by this peer, white =
//! server-owned, grey = another client's, amber = `client_owned` (the trust label: its owner's state is
//! accepted as is), red = a write was rejected in the last second. It shows the entities of every
//! session in the tree (so a test's server and client branches all appear). The same data is
//! `NetSession.overlay_rows()` / `Net.overlay_rows()`.

use crate::manager::OverlayEntry;
use crate::session::{NetSession, REJECTED_MS, SESSIONS_GROUP};
use godot::classes::{CanvasLayer, ICanvasLayer, Label, Node, Node2D, Node3D};
use godot::prelude::*;

/// The role of an entity on the peer `local` at `now_ms`.
pub fn role_of(e: &OverlayEntry, local: u32, now_ms: u64) -> &'static str {
    if e.rejected_ms > 0 && now_ms.saturating_sub(e.rejected_ms) < REJECTED_MS {
        "rejected"
    } else if e.client_owned {
        "client_owned"
    } else if e.owner == netcore::SERVER {
        "server"
    } else if e.owner == local {
        "own"
    } else {
        "other"
    }
}

/// The overlay colour of a role.
pub fn color_of(role: &str) -> Color {
    match role {
        "rejected" => Color::from_rgb(1.0, 0.25, 0.25),
        "client_owned" => Color::from_rgb(1.0, 0.72, 0.2),
        "server" => Color::from_rgb(1.0, 1.0, 1.0),
        "own" => Color::from_rgb(0.35, 1.0, 0.45),
        _ => Color::from_rgb(0.6, 0.6, 0.6),
    }
}

#[derive(GodotClass)]
#[class(init, base = CanvasLayer)]
pub struct NetOverlay {
    base: Base<CanvasLayer>,
    labels: Vec<Gd<Label>>,
}

#[godot_api]
impl ICanvasLayer for NetOverlay {
    fn ready(&mut self) {
        self.base_mut().set_layer(127);
    }

    fn process(&mut self, _delta: f64) {
        self.refresh();
    }
}

#[godot_api]
impl NetOverlay {
    /// Rebuilds the labels now (the overlay does it every frame).
    #[func]
    fn refresh(&mut self) {
        let Some(tree) = self.base().get_tree_or_null() else {
            return;
        };
        let mut placed: Vec<(Vector2, GString, Color)> = Vec::new();
        for n in tree.get_nodes_in_group(SESSIONS_GROUP).iter_shared() {
            let Ok(session) = n.try_cast::<NetSession>() else {
                continue;
            };
            let rows = session.bind().overlay_rows();
            for row in rows.iter_shared() {
                let Ok(d) = row.try_to::<VarDictionary>() else {
                    continue;
                };
                let Some(node) = d.get("node").and_then(|v| v.try_to::<Gd<Node>>().ok()) else {
                    continue;
                };
                let Some(pos) = screen_position(&node) else {
                    continue;
                };
                let id = d
                    .get("net_id")
                    .and_then(|v| v.try_to::<i64>().ok())
                    .unwrap_or(0);
                let owner = d
                    .get("owner")
                    .and_then(|v| v.try_to::<i64>().ok())
                    .unwrap_or(0);
                let color = d
                    .get("color")
                    .and_then(|v| v.try_to::<Color>().ok())
                    .unwrap_or(Color::WHITE);
                placed.push((
                    pos,
                    GString::from(format!("#{id} owner {owner}").as_str()),
                    color,
                ));
            }
        }
        while self.labels.len() < placed.len() {
            let mut l = Label::new_alloc();
            l.set_mouse_filter(godot::classes::control::MouseFilter::IGNORE);
            self.base_mut().add_child(&l);
            self.labels.push(l);
        }
        for (i, l) in self.labels.iter_mut().enumerate() {
            match placed.get(i) {
                Some((pos, text, color)) => {
                    l.set_text(text);
                    l.set_position(*pos + Vector2::new(6.0, -18.0));
                    l.set_modulate(*color);
                    l.set_visible(true);
                }
                None => l.set_visible(false),
            }
        }
    }
}

/// Where `node` (an entity root) is on screen, when it is a 2D or 3D node that can be placed.
fn screen_position(node: &Gd<Node>) -> Option<Vector2> {
    if !node.is_inside_tree() {
        return None;
    }
    if let Ok(n2) = node.clone().try_cast::<Node2D>() {
        return Some(n2.get_global_transform_with_canvas().origin);
    }
    let n3 = node.clone().try_cast::<Node3D>().ok()?;
    let cam = n3.get_viewport()?.get_camera_3d()?;
    let p = n3.get_global_position();
    if !cam.is_current() || cam.is_position_behind(p) {
        return None;
    }
    Some(cam.unproject_position(p))
}
