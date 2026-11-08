//! Part tables (docs/PLAN_NETCODE.md 2.1 "Identity", WP2): the parts of a scene are the nodes of a fresh
//! instance in depth-first order, each as (path relative to the instance root, class name). Nested
//! instances are parts too, except split ones (an instance with a `NetEntity` child, or whose scene is in
//! the manager's `split_scenes`), which are entities of their own and are left out of the walk with
//! everything below them. Both peers compute the table from the same `PackedScene`, so part index `i`
//! names the same node everywhere; the table's hash is compared at spawn.
//!
//! A bare node (not a scene instance, not inside one) is an entity whose "scene" is its class name and
//! whose only part is itself.
//!
//! Tables are computed once per (scene, split list) from a throwaway instance (`_init` of its scripts
//! runs, `_ready` does not) and cached on the main thread.

use crate::entity_node::NetEntity;
use crate::manager::NetManager;
use godot::classes::{ClassDb, Node, PackedScene, ResourceLoader};
use godot::prelude::*;
use netcore::PartTable;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A scene (or bare class) as gdnet spawns it.
#[derive(Debug)]
pub struct SceneInfo {
    /// The resource path, or the class name for a bare node.
    pub path: String,
    pub table: PartTable,
    /// `table.hash()`.
    pub parts_hash: u64,
    /// True for a bare node.
    pub bare: bool,
}

impl SceneInfo {
    pub fn bare(class: &str) -> Self {
        let table = PartTable::new(vec![(".".to_string(), class.to_string())]);
        Self {
            path: class.to_string(),
            parts_hash: table.hash(),
            table,
            bare: true,
        }
    }

    /// The node of part `index` under `root`.
    pub fn part_node(&self, root: &Gd<Node>, index: u16) -> Option<Gd<Node>> {
        let (path, _) = self.table.entries.get(usize::from(index))?;
        if path == "." {
            return Some(root.clone());
        }
        root.get_node_or_null(path.as_str())
    }

    /// Every part node that exists under `root`, in part order.
    pub fn part_nodes(&self, root: &Gd<Node>) -> Vec<Gd<Node>> {
        (0..self.table.len())
            .filter_map(|i| u16::try_from(i).ok())
            .filter_map(|i| self.part_node(root, i))
            .collect()
    }

    pub fn part_count(&self) -> u16 {
        u16::try_from(self.table.len()).unwrap_or(u16::MAX)
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<SceneInfo>>> = RefCell::new(HashMap::new());
}

/// True when `path` names a resource rather than a class.
pub fn is_resource_path(path: &str) -> bool {
    path.starts_with("res://") || path.starts_with("uid://") || path.starts_with("user://")
}

/// True when `node` is the root of an instance that is an entity of its own even inside another
/// entity's scene: it has a `NetEntity` child, or its scene is listed in `split`.
pub fn is_split_root(node: &Gd<Node>, split: &PackedStringArray) -> bool {
    let scene = node.get_scene_file_path();
    if scene.is_empty() {
        return false;
    }
    if split.as_slice().contains(&scene) {
        return true;
    }
    node.get_children()
        .iter_shared()
        .any(|c| c.try_cast::<NetEntity>().is_ok())
}

/// The part table of an instance root: depth-first, skipping split roots and nested managers.
pub fn walk(root: &Gd<Node>, split: &PackedStringArray) -> PartTable {
    let mut entries = vec![(".".to_string(), root.get_class().to_string())];
    fn visit(
        root: &Gd<Node>,
        node: &Gd<Node>,
        split: &PackedStringArray,
        out: &mut Vec<(String, String)>,
    ) {
        for child in node.get_children().iter_shared() {
            if child.clone().try_cast::<NetManager>().is_ok() || is_split_root(&child, split) {
                continue;
            }
            out.push((
                root.get_path_to(&child).to_string(),
                child.get_class().to_string(),
            ));
            visit(root, &child, split, out);
        }
    }
    visit(root, root, split, &mut entries);
    PartTable::new(entries)
}

fn cache_key(path: &str, split: &PackedStringArray) -> String {
    let mut key = path.to_string();
    for s in split.as_slice() {
        key.push('\u{1}');
        key.push_str(&s.to_string());
    }
    key
}

/// The scene at `path` (a resource path or a class name), loaded and walked once per split list.
/// `None` when it cannot be loaded or is not a Node scene / class.
pub fn scene_info(path: &str, split: &PackedStringArray) -> Option<Rc<SceneInfo>> {
    let key = cache_key(path, split);
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Some(hit);
    }
    let info = if is_resource_path(path) {
        let res = ResourceLoader::singleton().load(path)?;
        let packed = res.try_cast::<PackedScene>().ok()?;
        let instance = packed.instantiate()?;
        let table = walk(&instance, split);
        instance.free();
        SceneInfo {
            path: path.to_string(),
            parts_hash: table.hash(),
            table,
            bare: false,
        }
    } else {
        let db = ClassDb::singleton();
        if !db.class_exists(path) || !db.is_parent_class(path, "Node") {
            return None;
        }
        SceneInfo::bare(path)
    };
    let info = Rc::new(info);
    CACHE.with(|c| c.borrow_mut().insert(key, info.clone()));
    Some(info)
}

/// A new node of `info`: the scene instantiated, or the bare class created.
pub fn instantiate(info: &SceneInfo) -> Option<Gd<Node>> {
    if info.bare {
        ClassDb::singleton()
            .instantiate(info.path.as_str())
            .try_to::<Gd<Node>>()
            .ok()
    } else {
        ResourceLoader::singleton()
            .load(info.path.as_str())?
            .try_cast::<PackedScene>()
            .ok()?
            .instantiate()
    }
}
