//! `Net.configure(node, Dictionary)` (docs/PLAN_NETCODE.md 1 rung 3, 3.1, WP3): corrections to the
//! inferred property table of a scene, and per-entity options.
//!
//! Table changes are recorded per scene (the scene file of the entity `node` belongs to, or a bare node's
//! class) and applied when the scene's table is derived, which happens at its first spawn on each peer.
//! Both peers must make the same calls (the same scripts run everywhere), so the tables come out
//! identical; their hash is compared and a difference disconnects naming the scene and property. A table
//! change after the scene's table was derived on this peer is refused with an error.
//!
//! Keys: `state: ["+prop", "-prop", "+Part:prop"]`, `quantize: {prop: "cm"}`, `send: {prop: "hot"}`,
//! `tolerance: float | {prop: float}`, `interpolate: bool | {prop: bool}`, WP5 `teleport_distance: float |
//! {prop: float}`, WP16 `audience: {prop: "owner" | "all"}` (table changes); `priority`, WP16 `radius`
//! and `visible_to` (per entity, any time: interest.rs), WP19 `body_prediction` (per entity, this peer's
//! display choice, any time: predict_body.rs); anything else (`lag_compensated`, ...) is
//! kept for later packages in the root's `_net_config` meta.

use netcore::{Quant, Schedule};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// A property path relative to the entity root: (part path, property). The part path is `"."` for the
/// root.
pub type PropPath = (String, String);

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Add(PropPath),
    Remove(PropPath),
    Quantize(PropPath, Quant),
    Send(PropPath, Schedule),
    /// None = every property.
    Tolerance(Option<PropPath>, f32),
    Interpolate(Option<PropPath>, bool),
    /// WP5: the teleport distance (None = every property).
    Teleport(Option<PropPath>, f32),
    /// WP16: who receives the property (`audience: {prop: "owner"}`).
    Audience(PropPath, netcore::Audience),
}

thread_local! {
    static OPS: RefCell<HashMap<String, Vec<Op>>> = RefCell::new(HashMap::new());
    static BUILT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Splits `"Part:prop"` / `"prop"` (with an optional part prefix for a configure on a part node).
pub fn parse_path(text: &str, prefix: &str) -> PropPath {
    let (part, prop) = match text.find(':') {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => ("", text),
    };
    let part = part.trim_start_matches("./");
    let joined = match (prefix, part) {
        ("" | ".", "" | ".") => ".".to_string(),
        ("" | ".", p) => p.to_string(),
        (pre, "" | ".") => pre.to_string(),
        (pre, p) => format!("{pre}/{p}"),
    };
    (joined, prop.to_string())
}

/// Ops recorded for a scene key.
pub fn ops_for(key: &str) -> Vec<Op> {
    OPS.with(|o| o.borrow().get(key).cloned().unwrap_or_default())
}

/// True once this peer derived the scene's table.
fn is_built(key: &str) -> bool {
    BUILT.with(|b| b.borrow().contains(key))
}

pub fn mark_built(key: &str) {
    BUILT.with(|b| {
        b.borrow_mut().insert(key.to_string());
    });
}

/// Records table ops for `key`. Err when the table is already derived.
pub fn add_ops(key: &str, ops: Vec<Op>) -> Result<(), String> {
    if ops.is_empty() {
        return Ok(());
    }
    if is_built(key) {
        return Err(format!(
            "gdnet: Net.configure() would change the property table of {key}, which is fixed once its first entity spawned on this peer; call configure() before the first spawn (for example in the level's _ready, on every peer)"
        ));
    }
    OPS.with(|o| {
        o.borrow_mut()
            .entry(key.to_string())
            .or_default()
            .extend(ops)
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(parse_path("phase", ""), (".".into(), "phase".into()));
        assert_eq!(
            parse_path("Sprite:frame", ""),
            ("Sprite".into(), "frame".into())
        );
        assert_eq!(
            parse_path(".:position", ""),
            (".".into(), "position".into())
        );
        assert_eq!(
            parse_path("frame", "Sprite"),
            ("Sprite".into(), "frame".into())
        );
        assert_eq!(
            parse_path("Inner:x", "Sprite"),
            ("Sprite/Inner".into(), "x".into())
        );
    }
}
