//! Message framing (docs/PLAN_NETCODE.md 2.4): one kind byte, then the message body in `postcard`.
//! WP1 defines the session messages: `Hello` / `Welcome` / `Reject` (the handshake) and `Time`
//! ping/pong. Later packages add kinds; a receiver that does not know a kind reports it as an error.

use crate::entity::EntityAction;
use serde::{Deserialize, Serialize};
use std::string::String;
use std::vec::Vec;

/// gdnet's channels on the transport (2.3): 0 reliable ordered, 1 and 2 unreliable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Entity actions, calls, keyed state, the handshake.
    Reliable = 0,
    /// Inputs and snapshots.
    Unreliable = 1,
    /// Hashes and pings.
    Diagnostics = 2,
}

impl Channel {
    pub fn index(self) -> u8 {
        self as u8
    }

    pub fn reliability(self) -> Reliability {
        match self {
            Channel::Reliable => Reliability::Reliable,
            _ => Reliability::Unreliable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reliability {
    /// Reliable and ordered within its channel.
    Reliable,
    Unreliable,
}

/// One entry of the protocol description that `Hello` carries. Two peers are compatible when every
/// item is equal; the first differing one names the mismatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolItem {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// 64-bit hash of `items` (`Protocol::hash`).
    pub protocol_hash: u64,
    pub items: Vec<ProtocolItem>,
    pub tick_rate: u32,
}

/// A range of entity ids a peer may allocate from (a placeholder until WP2 allocates entities).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IdRange {
    pub start: u32,
    pub len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_hash: u64,
    pub tick_rate: u32,
    /// The joining peer's id, as the server's transport knows it.
    pub peer_id: u32,
    pub id_range: IdRange,
    /// The server's tick when it sent the welcome.
    pub current_tick: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub reason: String,
}

/// `Time` ping: the sender's clock (microseconds) and tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub sent_us: u64,
    pub tick: u64,
}

/// `Time` pong: the ping's `sent_us` echoed, and the responder's tick when it answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub echo_us: u64,
    pub tick: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    Welcome(Welcome),
    Reject(Reject),
    Ping(Ping),
    Pong(Pong),
    /// WP2: an entity action (create, destroy, reparent, set owner, scene announcement).
    Entity(EntityAction),
}

/// The kind bytes. 0 is never used, so a zeroed buffer is not a message.
pub mod kind {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const REJECT: u8 = 3;
    pub const PING: u8 = 4;
    pub const PONG: u8 = 5;
    pub const ENTITY: u8 = 6;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    Empty,
    UnknownKind(u8),
    /// The body of a known kind did not decode.
    Malformed(u8),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WireError::Empty => write!(f, "empty packet"),
            WireError::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            WireError::Malformed(k) => write!(f, "malformed message of kind {k}"),
        }
    }
}

impl Message {
    pub fn kind(&self) -> u8 {
        match self {
            Message::Hello(_) => kind::HELLO,
            Message::Welcome(_) => kind::WELCOME,
            Message::Reject(_) => kind::REJECT,
            Message::Ping(_) => kind::PING,
            Message::Pong(_) => kind::PONG,
            Message::Entity(_) => kind::ENTITY,
        }
    }

    /// The channel this message travels on.
    pub fn channel(&self) -> Channel {
        match self {
            Message::Hello(_) | Message::Welcome(_) | Message::Reject(_) | Message::Entity(_) => {
                Channel::Reliable
            }
            Message::Ping(_) | Message::Pong(_) => Channel::Diagnostics,
        }
    }
}

fn body<T: Serialize>(k: u8, value: &T) -> Vec<u8> {
    let mut out = std::vec![k];
    // Serialising these plain structs into a growable buffer cannot fail.
    out.extend(postcard::to_allocvec(value).unwrap_or_default());
    out
}

/// Frames a message: kind byte + postcard body.
pub fn encode(msg: &Message) -> Vec<u8> {
    match msg {
        Message::Hello(m) => body(kind::HELLO, m),
        Message::Welcome(m) => body(kind::WELCOME, m),
        Message::Reject(m) => body(kind::REJECT, m),
        Message::Ping(m) => body(kind::PING, m),
        Message::Pong(m) => body(kind::PONG, m),
        Message::Entity(m) => body(kind::ENTITY, m),
    }
}

fn parse<'a, T: Deserialize<'a>>(k: u8, rest: &'a [u8]) -> Result<T, WireError> {
    postcard::from_bytes(rest).map_err(|_| WireError::Malformed(k))
}

/// Unframes a message.
pub fn decode(bytes: &[u8]) -> Result<Message, WireError> {
    let (&k, rest) = bytes.split_first().ok_or(WireError::Empty)?;
    Ok(match k {
        kind::HELLO => Message::Hello(parse(k, rest)?),
        kind::WELCOME => Message::Welcome(parse(k, rest)?),
        kind::REJECT => Message::Reject(parse(k, rest)?),
        kind::PING => Message::Ping(parse(k, rest)?),
        kind::PONG => Message::Pong(parse(k, rest)?),
        kind::ENTITY => Message::Entity(parse(k, rest)?),
        other => return Err(WireError::UnknownKind(other)),
    })
}

/// The protocol description both ends must agree on (3.3). WP1 covers the netcore version, the tick
/// rate and a debug salt; later packages append scene part tables, profiles and option sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protocol {
    pub items: Vec<ProtocolItem>,
    /// WP2: the scenes this side registered, as (scene path, part table hash), in registration order.
    /// They are not part of `Hello` (part tables are computed when a scene is first spawned); they are
    /// compared per scene on first use with [`Protocol::scene_mismatch`].
    pub scenes: Vec<(String, u64)>,
}

impl Protocol {
    /// The WP1 protocol: netcore version, tick rate, and `salt` (0 normally; a test hook forces a
    /// mismatch with any other value).
    pub fn new(tick_rate: u32, salt: u64) -> Self {
        let item = |name: &str, value: String| ProtocolItem {
            name: name.into(),
            value,
        };
        Self {
            items: std::vec![
                item("netcore version", crate::VERSION.into()),
                item("tick rate", std::format!("{tick_rate}")),
                item("protocol salt", std::format!("{salt}")),
            ],
            scenes: Vec::new(),
        }
    }

    /// Records this side's part table hash for `path` (replacing an earlier one).
    pub fn register_scene(&mut self, path: &str, parts_hash: u64) {
        match self.scenes.iter_mut().find(|(p, _)| p == path) {
            Some(entry) => entry.1 = parts_hash,
            None => self.scenes.push((String::from(path), parts_hash)),
        }
    }

    /// This side's part table hash for `path`, if registered.
    pub fn scene(&self, path: &str) -> Option<u64> {
        self.scenes.iter().find(|(p, _)| p == path).map(|(_, h)| *h)
    }

    /// `None` when `theirs` equals this side's part table hash for `path` (or this side has not
    /// registered the scene); otherwise a reason naming the scene.
    pub fn scene_mismatch(
        &self,
        path: &str,
        theirs: u64,
        local_label: &str,
        remote_label: &str,
    ) -> Option<String> {
        let ours = self.scene(path)?;
        (ours != theirs).then(|| {
            std::format!(
                "protocol mismatch: the part table of scene {path} differs ({local_label} {ours:016x}, {remote_label} {theirs:016x}); both peers need the same version of the scene at the same path"
            )
        })
    }

    /// FNV-1a 64 over every item's name and value, each terminated by a zero byte.
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes.iter().chain(core::iter::once(&0u8)) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for it in &self.items {
            eat(it.name.as_bytes());
            eat(it.value.as_bytes());
        }
        h
    }

    /// `None` when `theirs` (a peer's items with their claimed hash) matches this protocol; otherwise a
    /// readable reason naming the first differing item, seen from this side as `local_label`.
    pub fn mismatch(
        &self,
        their_items: &[ProtocolItem],
        their_hash: u64,
        local_label: &str,
        remote_label: &str,
    ) -> Option<String> {
        if their_hash == self.hash() && their_items == self.items.as_slice() {
            return None;
        }
        let n = self.items.len().max(their_items.len());
        for i in 0..n {
            match (self.items.get(i), their_items.get(i)) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(a), Some(b)) if a.name == b.name => {
                    return Some(std::format!(
                        "protocol mismatch: {} differs ({local_label} {}, {remote_label} {})",
                        a.name,
                        a.value,
                        b.value
                    ));
                }
                (Some(a), Some(b)) => {
                    return Some(std::format!(
                        "protocol mismatch: item {i} differs ({local_label} has {}, {remote_label} has {})",
                        a.name, b.name
                    ));
                }
                (Some(a), None) => {
                    return Some(std::format!(
                        "protocol mismatch: {} is missing on the {remote_label}",
                        a.name
                    ))
                }
                (None, Some(b)) => {
                    return Some(std::format!(
                        "protocol mismatch: {} is missing on the {local_label}",
                        b.name
                    ))
                }
                (None, None) => unreachable!(),
            }
        }
        Some(String::from("protocol mismatch: protocol hash differs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message) {
        let bytes = encode(&msg);
        assert_eq!(bytes[0], msg.kind());
        assert_eq!(decode(&bytes), Ok(msg));
    }

    #[test]
    fn every_message_round_trips() {
        let p = Protocol::new(60, 0);
        roundtrip(Message::Hello(Hello {
            protocol_hash: p.hash(),
            items: p.items.clone(),
            tick_rate: 60,
        }));
        roundtrip(Message::Welcome(Welcome {
            protocol_hash: u64::MAX,
            tick_rate: 144,
            peer_id: 1_234_567_890,
            id_range: IdRange {
                start: 65_536,
                len: 65_536,
            },
            current_tick: 1 << 40,
        }));
        roundtrip(Message::Reject(Reject {
            reason: "nope ✗".into(),
        }));
        roundtrip(Message::Ping(Ping {
            sent_us: 123_456_789,
            tick: 7,
        }));
        roundtrip(Message::Pong(Pong {
            echo_us: 0,
            tick: u64::MAX,
        }));
        use crate::entity::{CreateAction, ParentRef};
        let parent = ParentRef {
            manager: u64::MAX,
            entity: 3,
            part: 2,
        };
        for action in [
            EntityAction::Scene {
                hash: 9,
                path: "res://a.tscn".into(),
                parts_hash: 77,
            },
            EntityAction::Create(CreateAction {
                id: 65_537,
                scene_hash: 9,
                parent,
                name: "Slime ✓".into(),
                owner: 4,
                parts: 3,
                initial_state: std::vec![1, 2, 3],
            }),
            EntityAction::Destroy { id: 1 },
            EntityAction::Reparent { id: 1, parent },
            EntityAction::SetOwner { id: 1, owner: 9 },
        ] {
            let msg = Message::Entity(action);
            assert_eq!(msg.channel(), Channel::Reliable);
            roundtrip(msg);
        }
    }

    #[test]
    fn scene_mismatch_names_the_scene() {
        let mut p = Protocol::new(60, 0);
        assert_eq!(
            p.scene_mismatch("res://a.tscn", 5, "client", "server"),
            None
        );
        p.register_scene("res://a.tscn", 5);
        assert_eq!(p.scene("res://a.tscn"), Some(5));
        assert_eq!(
            p.scene_mismatch("res://a.tscn", 5, "client", "server"),
            None
        );
        let reason = p
            .scene_mismatch("res://a.tscn", 6, "client", "server")
            .unwrap();
        assert!(
            reason.starts_with("protocol mismatch: the part table of scene res://a.tscn differs"),
            "{reason}"
        );
        // Scenes are compared per scene, not in the Hello hash.
        assert_eq!(p.hash(), Protocol::new(60, 0).hash());
    }

    #[test]
    fn small_messages_are_small() {
        let ping = encode(&Message::Ping(Ping {
            sent_us: 1_000_000,
            tick: 600,
        }));
        assert!(ping.len() <= 8, "{} bytes", ping.len());
    }

    #[test]
    fn bad_input_is_an_error() {
        assert_eq!(decode(&[]), Err(WireError::Empty));
        assert_eq!(decode(&[0]), Err(WireError::UnknownKind(0)));
        assert_eq!(decode(&[200, 1, 2]), Err(WireError::UnknownKind(200)));
        assert_eq!(
            decode(&[kind::WELCOME, 1]),
            Err(WireError::Malformed(kind::WELCOME))
        );
        let mut hello = encode(&Message::Hello(Hello {
            protocol_hash: 1,
            items: std::vec![],
            tick_rate: 60,
        }));
        hello.truncate(hello.len() - 1);
        assert_eq!(decode(&hello), Err(WireError::Malformed(kind::HELLO)));
    }

    #[test]
    fn channels() {
        assert_eq!(
            Message::Hello(Hello {
                protocol_hash: 0,
                items: std::vec![],
                tick_rate: 1
            })
            .channel(),
            Channel::Reliable
        );
        assert_eq!(
            Message::Ping(Ping {
                sent_us: 0,
                tick: 0
            })
            .channel()
            .index(),
            2
        );
        assert_eq!(Channel::Reliable.reliability(), Reliability::Reliable);
        assert_eq!(Channel::Unreliable.reliability(), Reliability::Unreliable);
    }

    #[test]
    fn protocol_hash_and_mismatch() {
        let a = Protocol::new(60, 0);
        let b = Protocol::new(60, 0);
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a.mismatch(&b.items, b.hash(), "server", "client"), None);
        let salted = Protocol::new(60, 7);
        assert_ne!(a.hash(), salted.hash());
        assert_eq!(
            a.mismatch(&salted.items, salted.hash(), "server", "client")
                .as_deref(),
            Some("protocol mismatch: protocol salt differs (server 0, client 7)")
        );
        // The first differing item is named, even when several differ.
        let slow = Protocol::new(30, 7);
        assert_eq!(
            a.mismatch(&slow.items, slow.hash(), "server", "client")
                .as_deref(),
            Some("protocol mismatch: tick rate differs (server 60, client 30)")
        );
        // Same items, forged hash.
        assert_eq!(
            a.mismatch(&b.items, 1, "server", "client").as_deref(),
            Some("protocol mismatch: protocol hash differs")
        );
        let mut short = a.items.clone();
        short.pop();
        let p = Protocol {
            items: short,
            scenes: Vec::new(),
        };
        assert_eq!(
            a.mismatch(&p.items, p.hash(), "server", "client")
                .as_deref(),
            Some("protocol mismatch: protocol salt is missing on the client")
        );
    }
}
