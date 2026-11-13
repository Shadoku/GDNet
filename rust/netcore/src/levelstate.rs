//! Level state blobs (docs/PLAN_NETCODE.md 12.2 "Level state", decision D20; WP16).
//!
//! When the server unloads a level whose managers keep their state (gdnet's `NetManager.keep_state`),
//! gdnet captures a [`LevelState`] and keeps its bytes in memory per level scene path; the next load of
//! that level restores it before the level's entities are announced. The same bytes are what
//! `NetSession.level_state(path)` hands the game for its own save file and what
//! `load_level(path, origin, state)` takes back. Nothing here goes on the wire: the format is local to
//! one game build and its saves.
//!
//! A blob is [`MAGIC`], the format version (`u16`, little endian, [`LEVEL_STATE_VERSION`]), then the
//! postcard encoding of the state with its strings interned (scene paths, part paths and property
//! names repeat across entities, so they are stored once). A blob with another magic or version is
//! refused ([`LevelStateError`]). Version 2 (WP17) records an entity's owner as a seat instead of a
//! peer id; version 1 blobs (WP16) are refused, since a peer id does not survive a save.
//!
//! Content: per manager (by its path relative to the level root), the entities to restore and the
//! addresses of the level's own entities that were destroyed while it was loaded. An entity is
//! addressed by its node path relative to the manager's managed root. A *scene* entity (`spawned` =
//! false) is part of the level scene and is matched by its address; a *spawned* one was added at run
//! time and is instantiated again from its scene under the node its address names as parent. Values are
//! by part path and property name (not by index), so a blob survives a property added to a scene; they
//! are the property values themselves (not quantised). `owner_seat` is the seat of the entity's owner
//! when the state was captured (0: the server, or an owner without a seat): gdnet restores every
//! entity owned by the server and gives it to that seat's peer once the seat is connected (WP17).

use crate::props::Value;
use crate::rooms::Origin;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::string::String;
use std::vec::Vec;

/// The first four bytes of every level state blob.
pub const MAGIC: [u8; 4] = *b"GDLS";
/// The format version this build writes (and the newest it reads).
pub const LEVEL_STATE_VERSION: u16 = 2;

/// The state of one level: its scene path and its managers' entities.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LevelState {
    /// The level's scene path (a blob is refused for another scene).
    pub scene: String,
    /// Where the level's root was when the state was captured: restored positions of the entities
    /// that do not inherit the root's transform move by the difference to its new place.
    pub origin: Option<Origin>,
    pub managers: Vec<ManagerState>,
}

/// The kept entities of one manager.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ManagerState {
    /// The manager's path relative to the level root ("NetManager").
    pub path: String,
    /// Scene entities with their state, and spawned entities, parents before children.
    pub entities: Vec<EntityState>,
    /// Addresses of the level's own entities destroyed while it was loaded (freed again on restore).
    pub removed: Vec<String>,
}

/// One entity.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct EntityState {
    /// Its node path relative to the manager's managed root ("Chest", "Chest/Items/Coin").
    pub address: String,
    /// Its scene path (a bare node: its class name).
    pub scene: String,
    /// Added at run time: instantiated again on restore. False: part of the level scene, matched by
    /// its address.
    pub spawned: bool,
    /// The seat of its owner when the state was captured; 0 = the server (or an owner without a seat).
    /// Restored entities belong to the server until that seat is connected.
    pub owner_seat: u32,
    pub parts: Vec<PartValues>,
}

/// The replicated values of one part of an entity.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PartValues {
    /// The part's path inside the entity ("." for its root).
    pub part: String,
    /// (property name, value).
    pub fields: Vec<(String, Value)>,
}

/// Why a blob was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LevelStateError {
    /// It does not start with [`MAGIC`] (not a level state blob, or empty).
    NotLevelState,
    /// A format version this build does not read.
    UnknownVersion(u16),
    /// The header is right but the rest does not decode.
    Corrupt(String),
}

impl fmt::Display for LevelStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLevelState => write!(f, "it is not a level state (no GDLS header)"),
            Self::UnknownVersion(v) => write!(
                f,
                "its format version is {v}; this build reads version {LEVEL_STATE_VERSION}"
            ),
            Self::Corrupt(why) => write!(f, "it does not decode ({why})"),
        }
    }
}

impl EntityState {
    /// The address of the node it hangs under ("" for the managed root).
    pub fn parent_address(&self) -> &str {
        self.address.rsplit_once('/').map_or("", |(p, _)| p)
    }

    /// Its node name (the last segment of its address).
    pub fn name(&self) -> &str {
        self.address
            .rsplit_once('/')
            .map_or(self.address.as_str(), |(_, n)| n)
    }
}

impl ManagerState {
    /// The entities in an order where every node an address names as parent comes before it:
    /// shallower addresses first, the captured order kept among equals.
    pub fn in_restore_order(&self) -> Vec<&EntityState> {
        let mut out: Vec<&EntityState> = self.entities.iter().collect();
        out.sort_by_key(|e| e.address.matches('/').count());
        out
    }
}

impl LevelState {
    pub fn new(scene: &str) -> Self {
        Self {
            scene: scene.into(),
            origin: None,
            managers: Vec::new(),
        }
    }

    /// The kept state of the manager at `path`, if any.
    pub fn manager(&self, path: &str) -> Option<&ManagerState> {
        self.managers.iter().find(|m| m.path == path)
    }

    /// Entities over all managers.
    pub fn entity_count(&self) -> usize {
        self.managers.iter().map(|m| m.entities.len()).sum()
    }

    /// The blob: header, then the interned postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        let mut strings = Strings::default();
        let wire = Wire {
            scene: strings.id(&self.scene),
            origin: self.origin,
            managers: self
                .managers
                .iter()
                .map(|m| WireManager {
                    path: strings.id(&m.path),
                    removed: m.removed.iter().map(|r| strings.id(r)).collect(),
                    entities: m
                        .entities
                        .iter()
                        .map(|e| WireEntity {
                            address: strings.id(&e.address),
                            scene: strings.id(&e.scene),
                            spawned: e.spawned,
                            owner_seat: e.owner_seat,
                            parts: e
                                .parts
                                .iter()
                                .map(|p| WirePart {
                                    part: strings.id(&p.part),
                                    fields: p
                                        .fields
                                        .iter()
                                        .map(|(n, v)| (strings.id(n), v.clone()))
                                        .collect(),
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
            strings: Vec::new(),
        };
        let wire = Wire {
            strings: strings.list,
            ..wire
        };
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&LEVEL_STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&postcard::to_allocvec(&wire).unwrap_or_default());
        out
    }

    /// Reads a blob written by [`LevelState::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, LevelStateError> {
        let version = version_of(bytes).ok_or(LevelStateError::NotLevelState)?;
        if version != LEVEL_STATE_VERSION {
            return Err(LevelStateError::UnknownVersion(version));
        }
        let wire: Wire = postcard::from_bytes(&bytes[6..])
            .map_err(|e| LevelStateError::Corrupt(std::format!("{e}")))?;
        let s = |i: u32| -> Result<String, LevelStateError> {
            wire.strings
                .get(i as usize)
                .cloned()
                .ok_or_else(|| LevelStateError::Corrupt(std::format!("no string {i}")))
        };
        let mut managers = Vec::with_capacity(wire.managers.len());
        for m in &wire.managers {
            let mut entities = Vec::with_capacity(m.entities.len());
            for e in &m.entities {
                let mut parts = Vec::with_capacity(e.parts.len());
                for p in &e.parts {
                    let mut fields = Vec::with_capacity(p.fields.len());
                    for (n, v) in &p.fields {
                        fields.push((s(*n)?, v.clone()));
                    }
                    parts.push(PartValues {
                        part: s(p.part)?,
                        fields,
                    });
                }
                entities.push(EntityState {
                    address: s(e.address)?,
                    scene: s(e.scene)?,
                    spawned: e.spawned,
                    owner_seat: e.owner_seat,
                    parts,
                });
            }
            managers.push(ManagerState {
                path: s(m.path)?,
                entities,
                removed: m.removed.iter().map(|r| s(*r)).collect::<Result<_, _>>()?,
            });
        }
        Ok(Self {
            scene: s(wire.scene)?,
            origin: wire.origin,
            managers,
        })
    }
}

/// The format version of a blob, None when it does not start with [`MAGIC`].
pub fn version_of(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 6 || bytes[..4] != MAGIC {
        return None;
    }
    Some(u16::from_le_bytes([bytes[4], bytes[5]]))
}

#[derive(Default)]
struct Strings {
    list: Vec<String>,
    ids: HashMap<String, u32>,
}

impl Strings {
    fn id(&mut self, s: &str) -> u32 {
        if let Some(i) = self.ids.get(s) {
            return *i;
        }
        let i = self.list.len() as u32;
        self.list.push(s.into());
        self.ids.insert(s.into(), i);
        i
    }
}

/// Version 2's body.
#[derive(Serialize, Deserialize)]
struct Wire {
    strings: Vec<String>,
    scene: u32,
    origin: Option<Origin>,
    managers: Vec<WireManager>,
}

#[derive(Serialize, Deserialize)]
struct WireManager {
    path: u32,
    removed: Vec<u32>,
    entities: Vec<WireEntity>,
}

#[derive(Serialize, Deserialize)]
struct WireEntity {
    address: u32,
    scene: u32,
    spawned: bool,
    owner_seat: u32,
    parts: Vec<WirePart>,
}

#[derive(Serialize, Deserialize)]
struct WirePart {
    part: u32,
    fields: Vec<(u32, Value)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;
    use std::vec;

    const CHEST: &str = "res://levels/chest.tscn";
    const COIN: &str = "res://levels/coin.tscn";

    fn part(path: &str, fields: &[(&str, Value)]) -> PartValues {
        PartValues {
            part: path.to_string(),
            fields: fields
                .iter()
                .map(|(n, v)| (n.to_string(), v.clone()))
                .collect(),
        }
    }

    fn entity(address: &str, scene: &str, spawned: bool, parts: Vec<PartValues>) -> EntityState {
        EntityState {
            address: address.to_string(),
            scene: scene.to_string(),
            spawned,
            owner_seat: 1,
            parts,
        }
    }

    fn sample() -> LevelState {
        let mut s = LevelState::new("res://levels/vault.tscn");
        s.origin = Some(Origin::Flat([4000.0, 0.0]));
        s.managers.push(ManagerState {
            path: "NetManager".into(),
            entities: vec![
                entity(
                    "Chest",
                    CHEST,
                    false,
                    vec![
                        part(
                            ".",
                            &[
                                ("open", Value::Bool(true)),
                                ("position", Value::Vector2([10.0, -4.5])),
                            ],
                        ),
                        part("Label", &[("text", Value::Str("gold".into()))]),
                    ],
                ),
                entity(
                    "Dropped",
                    COIN,
                    true,
                    vec![part(".", &[("value", Value::Int(7))])],
                ),
                entity(
                    "Chest/Items/Inside",
                    COIN,
                    true,
                    vec![part(".", &[("value", Value::Int(9))])],
                ),
            ],
            removed: vec!["Coin".into()],
        });
        s
    }

    #[test]
    fn a_level_state_round_trips() {
        let s = sample();
        let bytes = s.encode();
        assert_eq!(&bytes[..4], b"GDLS");
        assert_eq!(version_of(&bytes), Some(LEVEL_STATE_VERSION));
        let back = LevelState::decode(&bytes).expect("decodes");
        assert_eq!(back, s);
        assert_eq!(back.origin, Some(Origin::Flat([4000.0, 0.0])));
        assert_eq!(back.entity_count(), 3);
        assert_eq!(
            back.manager("NetManager").map(|m| m.removed.clone()),
            Some(vec!["Coin".to_string()])
        );
        assert!(back.manager("Other").is_none());
        // Every value kind survives unquantised.
        let mut all = LevelState::new("res://x.tscn");
        all.origin = Some(Origin::Deep([1.0, 2.0, 3.0]));
        let values = vec![
            Value::Nil,
            Value::Bool(false),
            Value::Int(-1 << 40),
            Value::Float(0.1),
            Value::Vector3([1.0, 2.0, 3.000_001]),
            Value::Vector4([1.0, 2.0, 3.0, 4.0]),
            Value::Vector2i([-3, 4]),
            Value::Vector3i([5, -6, 7]),
            Value::Quaternion([0.0, 0.0, 0.0, 1.0]),
            Value::Transform2D([1.0, 0.0, 0.0, 1.0, 5.0, 6.0]),
            Value::Transform3D([1.0; 12]),
            Value::Color([0.1, 0.2, 0.3, 1.0]),
            Value::Bytes(vec![0, 1, 2, 255]),
        ];
        let fields: Vec<(String, Value)> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (std::format!("f{i}"), v.clone()))
            .collect();
        all.managers.push(ManagerState {
            path: "M".into(),
            entities: vec![EntityState {
                address: "E".into(),
                scene: "Node2D".into(),
                spawned: false,
                owner_seat: 7,
                parts: vec![PartValues {
                    part: ".".into(),
                    fields,
                }],
            }],
            removed: vec![],
        });
        assert_eq!(LevelState::decode(&all.encode()), Ok(all));
    }

    #[test]
    fn an_empty_level_state_round_trips() {
        let s = LevelState::new("res://empty.tscn");
        assert_eq!(LevelState::decode(&s.encode()), Ok(s));
    }

    #[test]
    fn strings_are_stored_once() {
        let mut s = LevelState::new("res://levels/many.tscn");
        let mut m = ManagerState {
            path: "NetManager".into(),
            ..ManagerState::default()
        };
        for i in 0..100 {
            m.entities.push(entity(
                &std::format!("Coin{i}"),
                COIN,
                true,
                vec![part(
                    ".",
                    &[
                        ("value", Value::Int(i)),
                        ("position", Value::Vector2([i as f32, 0.0])),
                    ],
                )],
            ));
        }
        s.managers.push(m);
        let bytes = s.encode();
        let scene_mentions = bytes
            .windows(COIN.len())
            .filter(|w| *w == COIN.as_bytes())
            .count();
        assert_eq!(scene_mentions, 1, "the scene path is stored once");
        // Per entity: its address (~7 bytes with the length), ~8 bytes of indices and counts, and the
        // two values with their tags (~13); the shared strings come once.
        assert!(bytes.len() < 100 * 32, "{} bytes", bytes.len());
        assert_eq!(LevelState::decode(&bytes), Ok(s));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut bytes = sample().encode();
        bytes[4] = 9;
        bytes[5] = 0;
        assert_eq!(
            LevelState::decode(&bytes),
            Err(LevelStateError::UnknownVersion(9))
        );
        let msg = LevelState::decode(&bytes).unwrap_err().to_string();
        assert!(msg.contains("version is 9"), "{msg}");
    }

    #[test]
    fn other_bytes_are_refused() {
        assert_eq!(LevelState::decode(&[]), Err(LevelStateError::NotLevelState));
        assert_eq!(
            LevelState::decode(b"hello world"),
            Err(LevelStateError::NotLevelState)
        );
        let bytes = sample().encode();
        let cut = &bytes[..bytes.len() / 2];
        assert!(matches!(
            LevelState::decode(cut),
            Err(LevelStateError::Corrupt(_))
        ));
        // A string index past the table is corrupt, not a panic.
        let mut bad = Vec::from(&bytes[..6]);
        bad.extend_from_slice(
            &postcard::to_allocvec(&Wire {
                strings: vec![],
                scene: 3,
                origin: None,
                managers: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(
            LevelState::decode(&bad),
            Err(LevelStateError::Corrupt(_))
        ));
    }

    #[test]
    fn removed_content_and_runtime_spawns_are_recorded() {
        let back = LevelState::decode(&sample().encode()).unwrap();
        let m = back.manager("NetManager").unwrap();
        assert_eq!(m.removed, vec!["Coin".to_string()], "the picked-up coin");
        let spawned: Vec<&str> = m
            .entities
            .iter()
            .filter(|e| e.spawned)
            .map(|e| e.address.as_str())
            .collect();
        assert_eq!(spawned, vec!["Dropped", "Chest/Items/Inside"]);
        let inside = m.entities.iter().find(|e| e.name() == "Inside").unwrap();
        assert_eq!(inside.parent_address(), "Chest/Items");
        assert_eq!(inside.scene, COIN);
        assert_eq!(
            inside.parts[0].fields,
            vec![("value".into(), Value::Int(9))]
        );
        let dropped = m.entities.iter().find(|e| e.name() == "Dropped").unwrap();
        assert_eq!(dropped.parent_address(), "", "under the managed root");
        let chest = &m.entities[0];
        assert!(!chest.spawned, "the chest is level content");
    }

    #[test]
    fn restore_order_puts_parents_first() {
        let m = ManagerState {
            path: "NetManager".into(),
            entities: vec![
                entity("Bag/Items/Coin", COIN, true, vec![]),
                entity("Chest", CHEST, false, vec![]),
                entity("Bag", CHEST, true, vec![]),
                entity("Bag/Items/Coin/Gem", COIN, true, vec![]),
                entity("Lamp", CHEST, false, vec![]),
            ],
            removed: vec![],
        };
        let order: Vec<&str> = m
            .in_restore_order()
            .iter()
            .map(|e| e.address.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "Chest",
                "Bag",
                "Lamp",
                "Bag/Items/Coin",
                "Bag/Items/Coin/Gem"
            ]
        );
    }
}
