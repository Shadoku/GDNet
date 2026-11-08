//! Method tables (docs/PLAN_NETCODE.md 2.1 "Auto-exposed methods", WP6): which methods of an entity's
//! parts are network-callable, and how a method hash maps back to a name.
//!
//! Per part, the exposed methods are the part's script methods (every level of its script inheritance)
//! plus its class's methods from `ClassDB` (with inheritance), minus every name beginning with `_`
//! (virtuals, private helpers, `_net_allow` itself), and none at all on gdnet's own nodes (`NetEntity`,
//! `NetManager`). Names are sorted, each hashed with FNV-1a 32 (`netcore::call::method_hash`). Both
//! peers derive the table from the same scene and scripts, so nothing travels but a per-part hash of the
//! names, which joins the scene's property table in the `Scene` action: a peer whose scripts differ is
//! caught at the first spawn, naming the part.
//!
//! A method whose last declared parameter is typed `NetCallInfo` (read from the script's method list)
//! receives one appended to the arguments. Tables are computed once per scene from its first live
//! instance (after `_ready`) and cached on the main thread.

use crate::parts::SceneInfo;
use godot::classes::{ClassDb, Node, Script};
use godot::prelude::*;
use netcore::call::{method_hash, method_table_hash};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// The class name of the injected call info.
pub const CALL_INFO_CLASS: &str = "NetCallInfo";

/// gdnet's own node classes expose nothing.
const OWN_CLASSES: [&str; 2] = ["NetEntity", "NetManager"];

#[derive(Clone, Debug)]
pub struct MethodInfo {
    /// A Rust string: the table lives in a thread-local cache, dropped after the engine is gone.
    pub name: String,
    /// The last declared parameter is a `NetCallInfo`.
    pub wants_info: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PartMethods {
    /// Sorted names.
    pub names: Vec<String>,
    pub by_hash: HashMap<u32, MethodInfo>,
}

#[derive(Debug, Default)]
pub struct MethodTable {
    pub parts: Vec<PartMethods>,
}

impl MethodTable {
    /// The method of `part` with hash `hash`.
    pub fn lookup(&self, part: u16, hash: u32) -> Option<&MethodInfo> {
        self.parts.get(usize::from(part))?.by_hash.get(&hash)
    }

    /// The per-part hashes of the names (what the `Scene` action carries). `extra` is the test hook
    /// `"<part path>:<method>"` that adds a method to one part on this side only.
    pub fn hashes(&self, part_names: &[String], extra: &str) -> Vec<u64> {
        let (extra_part, extra_name) = extra.split_once(':').unwrap_or(("", ""));
        self.parts
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if !extra_name.is_empty() && part_names.get(i).is_some_and(|n| n == extra_part) {
                    let mut names = p.names.clone();
                    names.push(extra_name.to_string());
                    names.sort();
                    method_table_hash(&names)
                } else {
                    method_table_hash(&p.names)
                }
            })
            .collect()
    }

    /// Per part, the method hashes (for netcore's check of received calls).
    pub fn lists(&self) -> Vec<Vec<u32>> {
        self.parts
            .iter()
            .map(|p| p.by_hash.keys().copied().collect())
            .collect()
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<MethodTable>>> = RefCell::new(HashMap::new());
}

/// The method table of `scene`, derived from the part nodes of a live instance the first time.
pub fn table_for(scene: &SceneInfo, nodes: &[Option<Gd<Node>>]) -> Rc<MethodTable> {
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&scene.path).cloned()) {
        return hit;
    }
    let table = Rc::new(MethodTable {
        parts: (0..scene.table.len())
            .map(|i| match nodes.get(i) {
                Some(Some(n)) => part_methods(n),
                _ => PartMethods::default(),
            })
            .collect(),
    });
    CACHE.with(|c| c.borrow_mut().insert(scene.path.clone(), table.clone()));
    table
}

/// The exposed methods of one node.
pub fn part_methods(node: &Gd<Node>) -> PartMethods {
    let class = node.get_class().to_string();
    if OWN_CLASSES.contains(&class.as_str()) {
        return PartMethods::default();
    }
    // name -> wants_info; the most derived declaration wins.
    let mut found: BTreeMap<String, bool> = BTreeMap::new();
    let mut script = node.get_script().and_then(|s| s.try_cast::<Script>().ok());
    while let Some(s) = script {
        for m in s.get_script_method_list().iter_shared() {
            let Some(name) = m.get("name").map(|v| v.to::<String>()) else {
                continue;
            };
            if name.starts_with('_') || found.contains_key(&name) {
                continue;
            }
            found.insert(name, last_param_is_info(&m));
        }
        script = s.get_base_script();
    }
    for m in ClassDb::singleton()
        .class_get_method_list(class.as_str())
        .iter_shared()
    {
        let Some(name) = m.get("name").map(|v| v.to::<String>()) else {
            continue;
        };
        if !name.starts_with('_') {
            found.entry(name).or_insert(false);
        }
    }
    let mut out = PartMethods::default();
    for (name, wants_info) in found {
        out.by_hash.insert(
            method_hash(&name),
            MethodInfo {
                name: name.clone(),
                wants_info,
            },
        );
        out.names.push(name);
    }
    out
}

fn last_param_is_info(m: &VarDictionary) -> bool {
    let Some(args) = m.get("args") else {
        return false;
    };
    // The list may be a typed `Array[Dictionary]` or an untyped one.
    let last = match args.try_to::<Array<VarDictionary>>() {
        Ok(a) => a.back(),
        Err(_) => args
            .try_to::<VarArray>()
            .ok()
            .and_then(|a| a.back())
            .and_then(|v| v.try_to::<VarDictionary>().ok()),
    };
    last.and_then(|d| d.get("class_name"))
        .is_some_and(|c| c.to_string() == CALL_INFO_CLASS)
}
