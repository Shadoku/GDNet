//! WP8: the editor side of the diagnostics (docs/PLAN_NETCODE.md 1 "What the developer sees", WP8).
//!
//! `GdNetEditorPlugin` (an `EditorPlugin`, auto-registered by gdext) adds `GdNetDebuggerPlugin`, an
//! `EditorDebuggerPlugin` that puts a "gdnet" tab into each debugger session. The running game's `Net`
//! session sends `gdnet:stats` (its `stats()`) and `gdnet:entities` (`overlay_rows()` without the
//! nodes) twice a second while `EngineDebugger.is_active()` (session.rs `feed_debugger`); the tab shows
//! them as text. The editor cannot run headless, so this part is verified by hand only; every editor
//! call is guarded by `Engine.is_editor_hint()`.

use godot::classes::{
    EditorDebuggerPlugin, EditorPlugin, Engine, IEditorDebuggerPlugin, IEditorPlugin, Label,
    ScrollContainer, VBoxContainer,
};
use godot::prelude::*;
use std::collections::HashMap;

pub const MSG_STATS: &str = "gdnet:stats";
pub const MSG_ENTITIES: &str = "gdnet:entities";

#[derive(GodotClass)]
#[class(tool, init, base = EditorPlugin)]
pub struct GdNetEditorPlugin {
    base: Base<EditorPlugin>,
    debugger: Option<Gd<GdNetDebuggerPlugin>>,
}

#[godot_api]
impl IEditorPlugin for GdNetEditorPlugin {
    fn enter_tree(&mut self) {
        if !Engine::singleton().is_editor_hint() {
            return;
        }
        let d = GdNetDebuggerPlugin::new_gd();
        self.base_mut().add_debugger_plugin(&d);
        self.debugger = Some(d);
    }

    fn exit_tree(&mut self) {
        if let Some(d) = self.debugger.take() {
            self.base_mut().remove_debugger_plugin(&d);
        }
    }
}

#[derive(GodotClass)]
#[class(tool, init, base = EditorDebuggerPlugin)]
pub struct GdNetDebuggerPlugin {
    base: Base<EditorDebuggerPlugin>,
    /// Per debugger session: the stats label and the entities label.
    tabs: HashMap<i32, (Gd<Label>, Gd<Label>)>,
}

#[godot_api]
impl IEditorDebuggerPlugin for GdNetDebuggerPlugin {
    fn setup_session(&mut self, session_id: i32) {
        if !Engine::singleton().is_editor_hint() {
            return;
        }
        let Some(mut session) = self.base().get_session(session_id) else {
            return;
        };
        let mut scroll = ScrollContainer::new_alloc();
        scroll.set_name("gdnet");
        let mut vbox = VBoxContainer::new_alloc();
        let mut stats = Label::new_alloc();
        stats.set_text("gdnet: waiting for the game's Net session…");
        let entities = Label::new_alloc();
        vbox.add_child(&stats);
        vbox.add_child(&entities);
        scroll.add_child(&vbox);
        session.add_session_tab(&scroll);
        self.tabs.insert(session_id, (stats, entities));
    }

    fn has_capture(&self, capture: GString) -> bool {
        capture == "gdnet"
    }

    fn capture(&mut self, message: GString, data: VarArray, session_id: i32) -> bool {
        let Some((stats, entities)) = self.tabs.get_mut(&session_id) else {
            return false;
        };
        let first = data.get(0).unwrap_or_default();
        match message.to_string().as_str() {
            MSG_STATS => {
                let Ok(d) = first.try_to::<VarDictionary>() else {
                    return false;
                };
                let mut lines: Vec<String> =
                    d.iter_shared().map(|(k, v)| format!("{k}: {v}")).collect();
                lines.sort();
                stats.set_text(&lines.join("\n"));
                true
            }
            MSG_ENTITIES => {
                let Ok(rows) = first.try_to::<VarArray>() else {
                    return false;
                };
                let lines: Vec<String> = rows
                    .iter_shared()
                    .filter_map(|r| r.try_to::<VarDictionary>().ok())
                    .map(|d| {
                        let g = |k: &str| d.get(k).map(|v| v.to_string()).unwrap_or_default();
                        format!(
                            "#{} owner {} {} {} diverged {}",
                            g("net_id"),
                            g("owner"),
                            g("role"),
                            g("scene"),
                            g("hashes_diverged")
                        )
                    })
                    .collect();
                entities.set_text(&format!(
                    "entities ({}):\n{}",
                    lines.len(),
                    lines.join("\n")
                ));
                true
            }
            _ => false,
        }
    }
}
