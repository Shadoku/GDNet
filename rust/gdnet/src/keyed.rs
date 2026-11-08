//! `NetKeyedState` (docs/PLAN_NETCODE.md 1 rung 2, 3.1, WP7; the protocol is netcore/src/keyed.rs):
//! `Net.state` / `NetSession.state`, a reliable map of paths to Variants.
//!
//! ```gdscript
//! Net.state.set("score/" + str(peer_id), 10)     # the server writes; every peer gets it
//! Net.state.allow("chat/", "all")                 # clients may write under chat/ too
//! Net.state.view("team/red/", func(peer): return peer in red_team)   # only those peers see it
//! Net.state.changed.connect(func(path, value): ...)
//! ```
//!
//! The map lives in the session's netcore (values are the WP6 Variant encoding, so Objects, Callables
//! and RIDs are refused). Without a running session (single player, or before `host()`), the object
//! keeps a local map with the same API and signals, and sends nothing (gate G0).
//!
//! `allow(prefix, who)`: `who` is a peer id, `"all"` or `"owner"` (a peer may write the paths whose
//! segment after the prefix is its own id: `allow("score/", "owner")` lets peer 5 write `score/5`).
//! A client's `set` on a path it may not write is refused at once with an error (nothing is sent); one
//! the server refuses anyway is rolled back and `write_rejected(path)` fires. `view(prefix, filter)`:
//! `filter(peer_id) -> bool` decides, per peer, whether the paths under `prefix` reach it; the server
//! re-evaluates it every tick.

use crate::call::{self as net_call};
use crate::session::NetSession;
use godot::prelude::*;
use netcore::KeyEvent;
use std::collections::BTreeMap;

#[derive(GodotClass)]
#[class(tool, init, base = RefCounted)]
pub struct NetKeyedState {
    base: Base<RefCounted>,
    /// The session whose map this is (None: the local map).
    session: Option<InstanceId>,
    local: BTreeMap<String, Variant>,
    /// Server views: (prefix, filter).
    views: Vec<(String, Callable)>,
}

#[godot_api]
impl NetKeyedState {
    /// A path changed (on every peer that sees it, the writer included).
    #[signal]
    fn changed(path: GString, value: Variant);
    /// A path was erased (or stopped being visible to this peer).
    #[signal]
    fn erased(path: GString);
    /// This peer's write of `path` was refused by the server and rolled back.
    #[signal]
    fn write_rejected(path: GString);

    /// Sets `path` to `value`. True when it was applied (a refused write prints an error).
    #[func(rename = set)]
    fn set_value(&mut self, path: GString, value: Variant) -> bool {
        let path = path.to_string();
        let encoded = match net_call::to_arg(&value) {
            Ok(a) => netcore::call::encode_value(&a),
            Err(e) => {
                godot_error!("gdnet: Net.state.set({path:?}): {e}");
                return false;
            }
        };
        match self.session_gd() {
            Some(mut s) => {
                let r = s
                    .bind_mut()
                    .keyed_op(|c, now| c.keyed_set(&path, encoded, now));
                self.finish(r, &path)
            }
            None => {
                if self.local.get(&path) != Some(&value) {
                    self.local.insert(path.clone(), value.clone());
                    self.base_mut().emit_signal(
                        "changed",
                        &[GString::from(path.as_str()).to_variant(), value],
                    );
                }
                true
            }
        }
    }

    /// The value at `path`, or `default`.
    #[func(rename = get)]
    fn get_value(
        &self,
        path: GString,
        #[opt(default = net_call::NetArg::absent())] default: net_call::NetArg,
    ) -> Variant {
        let path = path.to_string();
        let default = default.or_nil();
        match self.session_gd() {
            Some(s) => s
                .bind()
                .keyed_get(&path)
                .map_or(default, |b| net_call::decode_return(&b)),
            None => self.local.get(&path).cloned().unwrap_or(default),
        }
    }

    /// Erases `path`. True when it was applied.
    #[func]
    fn erase(&mut self, path: GString) -> bool {
        let path = path.to_string();
        match self.session_gd() {
            Some(mut s) => {
                let r = s.bind_mut().keyed_op(|c, now| c.keyed_erase(&path, now));
                self.finish(r, &path)
            }
            None => {
                if self.local.remove(&path).is_some() {
                    self.base_mut()
                        .emit_signal("erased", &[GString::from(path.as_str()).to_variant()]);
                }
                true
            }
        }
    }

    #[func]
    fn has(&self, path: GString) -> bool {
        let path = path.to_string();
        match self.session_gd() {
            Some(s) => s.bind().keyed_get(&path).is_some(),
            None => self.local.contains_key(&path),
        }
    }

    /// The paths that start with `prefix`, sorted.
    #[func]
    fn keys(&self, #[opt(default = "")] prefix: GString) -> PackedStringArray {
        let prefix = prefix.to_string();
        let keys: Vec<String> = match self.session_gd() {
            Some(s) => s.bind().keyed_keys(&prefix),
            None => self
                .local
                .keys()
                .filter(|k| k.starts_with(prefix.as_str()))
                .cloned()
                .collect(),
        };
        keys.iter().map(|k| GString::from(k.as_str())).collect()
    }

    /// Server: lets `who` (a peer id, `"all"` or `"owner"`) write the paths under `prefix`.
    #[func]
    fn allow(&mut self, prefix: GString, who: Variant) {
        let who = match who.get_type() {
            VariantType::INT => netcore::Who::Peer(who.to::<i64>().max(0) as u32),
            _ => match who.to_string().as_str() {
                "all" => netcore::Who::All,
                "owner" => netcore::Who::Owner,
                other => {
                    godot_error!(
                        "gdnet: Net.state.allow: who must be a peer id, \"all\" or \"owner\" (got {other:?})"
                    );
                    return;
                }
            },
        };
        let Some(mut s) = self.session_gd() else {
            return; // single player: everyone is the server
        };
        if !s.bind().is_server() {
            godot_error!("gdnet: Net.state.allow is for the server");
            return;
        }
        let prefix = prefix.to_string();
        s.bind_mut()
            .keyed_op(|c, now| {
                c.keyed_allow(&prefix, who, now);
                Ok(())
            })
            .ok();
    }

    /// Server: only the peers for which `filter(peer_id)` returns true see the paths under `prefix`
    /// (re-evaluated every tick).
    #[func]
    fn view(&mut self, prefix: GString, filter: Callable) {
        let prefix = prefix.to_string();
        self.views.retain(|(p, _)| *p != prefix);
        self.views.push((prefix.clone(), filter.clone()));
        let Some(mut s) = self.session_gd() else {
            return;
        };
        if !s.bind().is_server() {
            godot_error!("gdnet: Net.state.view is for the server");
            return;
        }
        let peers = s.bind().peers();
        let decisions: Vec<(u32, bool)> = {
            let _guard = self.base_mut();
            peers
                .as_slice()
                .iter()
                .map(|p| (*p as u32, call_filter(&filter, *p as u32)))
                .collect()
        };
        let events = s
            .bind_mut()
            .keyed_op(|c, now| {
                c.keyed_view(&prefix, &decisions, now);
                Ok(())
            })
            .unwrap_or_default();
        self.emit_events(events);
    }
}

/// The view filter's decision for `peer` (anything but true hides).
pub fn call_filter(filter: &Callable, peer: u32) -> bool {
    filter.is_valid()
        && filter
            .call(&[i64::from(peer).to_variant()])
            .try_to::<bool>()
            .unwrap_or(false)
}

impl NetKeyedState {
    /// Binds the object to `session` (None: back to the local map).
    pub fn attach(&mut self, session: Option<InstanceId>) {
        self.session = session;
    }

    pub fn views(&self) -> Vec<(String, Callable)> {
        self.views.clone()
    }

    fn session_gd(&self) -> Option<Gd<NetSession>> {
        let s = Gd::<NetSession>::try_from_instance_id(self.session?).ok()?;
        let live = s.bind().has_core();
        live.then_some(s)
    }

    fn finish(&mut self, r: Result<Vec<KeyEvent>, String>, path: &str) -> bool {
        match r {
            Ok(events) => {
                self.emit_events(events);
                true
            }
            Err(e) => {
                godot_error!("gdnet: Net.state ({path:?}): {e}");
                false
            }
        }
    }

    /// Emits the signals of `events` (under the base guard: handlers may call back in).
    pub fn emit_events(&mut self, events: Vec<KeyEvent>) {
        for e in events {
            match e {
                KeyEvent::Changed(p, v) => {
                    let value = net_call::decode_return(&v);
                    self.base_mut()
                        .emit_signal("changed", &[GString::from(p.as_str()).to_variant(), value]);
                }
                KeyEvent::Erased(p) => {
                    self.base_mut()
                        .emit_signal("erased", &[GString::from(p.as_str()).to_variant()]);
                }
                KeyEvent::Rejected(p) => {
                    self.base_mut()
                        .emit_signal("write_rejected", &[GString::from(p.as_str()).to_variant()]);
                }
            }
        }
    }
}
