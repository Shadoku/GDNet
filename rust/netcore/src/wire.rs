//! Message framing (docs/PLAN_NETCODE.md 2.4): one kind byte, then the message body in `postcard`.
//! WP1 defines the session messages: `Hello` / `Welcome` / `Reject` (the handshake) and `Time`
//! ping/pong. Later packages add kinds; a receiver that does not know a kind reports it as an error.

use crate::authority::{StateMsg, WriteReject};
use crate::call::{CallMsg, CallReturn};
use crate::delta::{Ack, FieldMsg};
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
    /// WP17: the client's seat key (seats.rs): a returning key takes its seat back.
    pub key: String,
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
    /// WP17: the joining peer's seat, and the seat table.
    pub seat: u32,
    pub seats: Vec<crate::seats::SeatInfo>,
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
    /// WP3: a snapshot body (bit-packed, see snapshot.rs; not postcard).
    Snapshot(Vec<u8>),
    /// WP3: the receiver's acknowledgement of snapshots.
    Ack(Ack),
    /// WP3: one field sent reliably.
    Field(FieldMsg),
    /// WP4: input frames, bit-packed (see input.rs; not postcard).
    Input(Vec<u8>),
    /// WP6: a method call on an entity's part (call.rs).
    Call(CallMsg),
    /// WP6: the answer to a call's request id.
    CallReturn(CallReturn),
    /// WP7: a `client_owned` entity's state from its owner (unreliable; authority.rs).
    OwnerState(StateMsg),
    /// WP7: a client's change of an entity it does not own (reliable; answered by `WriteReject`).
    Write(StateMsg),
    /// WP7: the server refused a write; the writer restores the server's values.
    WriteReject(WriteReject),
    /// WP7: keyed state (keyed.rs).
    Keyed(crate::keyed::KeyMsg),
    /// WP8: sampled per-entity state hashes (hash.rs; channel 2).
    Hash(crate::hash::HashMsg),
    /// WP17: the seat table changed (a seat taken, away, back or released).
    Seats(Vec<crate::seats::SeatInfo>),
}

/// The kind bytes. 0 is never used, so a zeroed buffer is not a message.
pub mod kind {
    pub const HELLO: u8 = 1;
    pub const WELCOME: u8 = 2;
    pub const REJECT: u8 = 3;
    pub const PING: u8 = 4;
    pub const PONG: u8 = 5;
    pub const ENTITY: u8 = 6;
    pub const SNAPSHOT: u8 = 7;
    pub const ACK: u8 = 8;
    pub const FIELD: u8 = 9;
    pub const INPUT: u8 = 10;
    pub const CALL: u8 = 11;
    pub const CALL_RETURN: u8 = 12;
    pub const OWNER_STATE: u8 = 13;
    pub const WRITE: u8 = 14;
    pub const WRITE_REJECT: u8 = 15;
    pub const KEYED: u8 = 16;
    pub const HASH: u8 = 17;
    pub const SEATS: u8 = 18;
}

/// The channel a message of kind `k` travels on (for byte counters of received packets).
pub fn channel_of_kind(k: u8) -> Channel {
    match k {
        kind::PING | kind::PONG | kind::HASH => Channel::Diagnostics,
        kind::SNAPSHOT | kind::ACK | kind::INPUT | kind::OWNER_STATE => Channel::Unreliable,
        _ => Channel::Reliable,
    }
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
            Message::Snapshot(_) => kind::SNAPSHOT,
            Message::Ack(_) => kind::ACK,
            Message::Field(_) => kind::FIELD,
            Message::Input(_) => kind::INPUT,
            Message::Call(_) => kind::CALL,
            Message::CallReturn(_) => kind::CALL_RETURN,
            Message::OwnerState(_) => kind::OWNER_STATE,
            Message::Write(_) => kind::WRITE,
            Message::WriteReject(_) => kind::WRITE_REJECT,
            Message::Keyed(_) => kind::KEYED,
            Message::Hash(_) => kind::HASH,
            Message::Seats(_) => kind::SEATS,
        }
    }

    /// The channel this message travels on.
    pub fn channel(&self) -> Channel {
        match self {
            Message::Hello(_)
            | Message::Welcome(_)
            | Message::Reject(_)
            | Message::Entity(_)
            | Message::Field(_)
            | Message::CallReturn(_)
            | Message::Write(_)
            | Message::WriteReject(_)
            | Message::Keyed(_)
            | Message::Seats(_) => Channel::Reliable,
            Message::Call(c) if c.unreliable => Channel::Unreliable,
            Message::Call(_) => Channel::Reliable,
            Message::Snapshot(_) | Message::Ack(_) | Message::Input(_) | Message::OwnerState(_) => {
                Channel::Unreliable
            }
            Message::Ping(_) | Message::Pong(_) | Message::Hash(_) => Channel::Diagnostics,
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
        Message::Snapshot(b) => {
            let mut out = Vec::with_capacity(b.len() + 1);
            out.push(kind::SNAPSHOT);
            out.extend_from_slice(b);
            out
        }
        Message::Ack(m) => body(kind::ACK, m),
        Message::Field(m) => body(kind::FIELD, m),
        Message::Input(b) => {
            let mut out = Vec::with_capacity(b.len() + 1);
            out.push(kind::INPUT);
            out.extend_from_slice(b);
            out
        }
        Message::Call(m) => body(kind::CALL, m),
        Message::CallReturn(m) => body(kind::CALL_RETURN, m),
        Message::OwnerState(m) => body(kind::OWNER_STATE, m),
        Message::Write(m) => body(kind::WRITE, m),
        Message::WriteReject(m) => body(kind::WRITE_REJECT, m),
        Message::Keyed(m) => body(kind::KEYED, m),
        Message::Hash(m) => body(kind::HASH, m),
        Message::Seats(m) => body(kind::SEATS, m),
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
        kind::SNAPSHOT => Message::Snapshot(rest.to_vec()),
        kind::ACK => Message::Ack(parse(k, rest)?),
        kind::FIELD => Message::Field(parse(k, rest)?),
        kind::INPUT => Message::Input(rest.to_vec()),
        kind::CALL => Message::Call(parse(k, rest)?),
        kind::CALL_RETURN => Message::CallReturn(parse(k, rest)?),
        kind::OWNER_STATE => Message::OwnerState(parse(k, rest)?),
        kind::WRITE => Message::Write(parse(k, rest)?),
        kind::WRITE_REJECT => Message::WriteReject(parse(k, rest)?),
        kind::KEYED => Message::Keyed(parse(k, rest)?),
        kind::HASH => Message::Hash(parse(k, rest)?),
        kind::SEATS => Message::Seats(parse(k, rest)?),
        other => return Err(WireError::UnknownKind(other)),
    })
}

/// The wire format version, an item of [`Protocol`]: raised whenever the bytes of an existing message
/// change meaning while the crate version stays. 1 = WP0-WP14; 2 = D17 (2D `mm`/`cm` steps of 0.1 px
/// and 1 px instead of 0.001 px and 0.01 px); 3 = WP16 (`Travel` carries a level origin, `RoomLeft`,
/// the `audience` property option); 4 = WP16 level state (`Travel` names the level content that is gone);
/// 5 = WP17 (`Hello` carries the seat key, `Welcome` the seat and the seat table, `Seats`); 6 = WP19
/// (`Create` carries the physics mode, `SetPhysics`).
pub const WIRE_FORMAT: u32 = 6;

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
        Self::with_profiles(tick_rate, salt, 0)
    }

    /// WP3 (3.3): the protocol with the hash of the effective profiles (0 = none given).
    pub fn with_profiles(tick_rate: u32, salt: u64, profiles_hash: u64) -> Self {
        let item = |name: &str, value: String| ProtocolItem {
            name: name.into(),
            value,
        };
        Self {
            items: std::vec![
                item("netcore version", crate::VERSION.into()),
                item("wire format", std::format!("{WIRE_FORMAT}")),
                item("tick rate", std::format!("{tick_rate}")),
                item("protocol salt", std::format!("{salt}")),
                item("profiles", std::format!("{profiles_hash:016x}")),
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
            key: "a save slot ✓".into(),
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
            seat: 3,
            seats: std::vec![crate::seats::SeatInfo {
                seat: 3,
                peer: 1_234_567_890,
                away: false
            }],
        }));
        roundtrip(Message::Seats(std::vec![
            crate::seats::SeatInfo {
                seat: 1,
                peer: 1,
                away: false
            },
            crate::seats::SeatInfo {
                seat: 2,
                peer: 77,
                away: true
            },
        ]));
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
                props: crate::props::ScenePropsTable {
                    parts: std::vec![std::vec![crate::props::PropDesc::new(
                        "position",
                        crate::props::TypeTag::Vector2
                    )]],
                    inputs: std::vec!["jump".into()],
                    methods: std::vec![7, 8],
                },
            },
            EntityAction::Create(CreateAction {
                id: 65_537,
                scene_hash: 9,
                parent,
                name: "Slime ✓".into(),
                owner: 4,
                parts: 3,
                initial_state: std::vec![1, 2, 3],
                ..CreateAction::default()
            }),
            EntityAction::Destroy { id: 1 },
            EntityAction::Reparent { id: 1, parent },
            EntityAction::SetOwner { id: 1, owner: 9 },
            EntityAction::RequestOwner { id: 1 },
            EntityAction::ReleaseOwner { id: 1 },
            EntityAction::OwnerQuery {
                id: 1,
                requester: 9,
            },
            EntityAction::OwnerAnswer {
                id: 1,
                requester: 9,
                grant: true,
            },
            EntityAction::OwnerReply {
                id: 1,
                granted: false,
            },
            EntityAction::Create(CreateAction {
                id: 2,
                transfer: crate::authority::Transfer::Take,
                client_owned: true,
                physics: crate::correct::PhysicsMode::Simulated,
                ..CreateAction::default()
            }),
            EntityAction::SetPhysics {
                id: 2,
                physics: crate::correct::PhysicsMode::Owner,
                client_owned: true,
            },
        ] {
            let msg = Message::Entity(action);
            assert_eq!(msg.channel(), Channel::Reliable);
            roundtrip(msg);
        }
        roundtrip(Message::Snapshot(std::vec![1, 2, 3]));
        roundtrip(Message::Ack(Ack {
            tick: 99,
            history: 0xdead_beef,
        }));
        roundtrip(Message::Field(FieldMsg {
            tick: 5,
            id: 3,
            part: 1,
            field: 2,
            bits: 9,
            data: std::vec![1, 2],
        }));
        roundtrip(Message::Input(std::vec![4, 5]));
        // WP7: owner state (unreliable), writes and rejects, keyed state (reliable).
        let st = crate::authority::StateMsg {
            tick: 7,
            entities: std::vec![(3, std::vec![1, 2])],
        };
        roundtrip(Message::OwnerState(st.clone()));
        assert_eq!(
            Message::OwnerState(st.clone()).channel(),
            Channel::Unreliable
        );
        assert_eq!(channel_of_kind(kind::OWNER_STATE), Channel::Unreliable);
        roundtrip(Message::Write(st));
        roundtrip(Message::WriteReject(crate::authority::WriteReject {
            id: 3,
            fields: std::vec![(0, 2)],
        }));
        for m in [
            crate::keyed::KeyMsg::Set {
                path: "score/5".into(),
                value: std::vec![1],
            },
            crate::keyed::KeyMsg::Erase { path: "a".into() },
            crate::keyed::KeyMsg::Full {
                entries: std::vec![("a".into(), std::vec![])],
                rules: std::vec![("chat/".into(), crate::keyed::Who::All)],
            },
            crate::keyed::KeyMsg::Rules {
                rules: std::vec![("s/".into(), crate::keyed::Who::Peer(4))],
            },
            crate::keyed::KeyMsg::Rejected {
                path: "x".into(),
                value: None,
            },
        ] {
            assert_eq!(Message::Keyed(m.clone()).channel(), Channel::Reliable);
            roundtrip(Message::Keyed(m));
        }
        assert_eq!(Message::Input(Vec::new()).channel(), Channel::Unreliable);
        assert_eq!(channel_of_kind(kind::INPUT), Channel::Unreliable);
        assert_eq!(channel_of_kind(kind::SNAPSHOT), Channel::Unreliable);
        assert_eq!(channel_of_kind(kind::FIELD), Channel::Reliable);
        assert_eq!(channel_of_kind(kind::PONG), Channel::Diagnostics);
        // WP8: hashes on channel 2.
        let hm = Message::Hash(crate::hash::HashMsg {
            tick: 9,
            settled: std::vec![(3, 0xdead_beef)],
            carried: std::vec![(4, 1)],
        });
        assert_eq!(hm.channel(), Channel::Diagnostics);
        assert_eq!(channel_of_kind(kind::HASH), Channel::Diagnostics);
        assert_eq!(decode(&encode(&hm)), Ok(hm));
        let ack = encode(&Message::Ack(Ack {
            tick: 600,
            history: 0,
        }));
        assert!(ack.len() <= 5, "{} bytes", ack.len());
        // WP6: calls and returns; a call's channel follows its `unreliable` flag.
        let call = crate::call::CallMsg {
            entity: 65_537,
            part: 2,
            method: crate::call::method_hash("open_door"),
            verb: crate::call::Verb::Peer,
            target: 7,
            sender: 3,
            request: 42,
            tick: 1 << 33,
            unreliable: false,
            args: crate::call::encode_args(&[crate::call::Arg::Int(7)]),
        };
        let msg = Message::Call(call.clone());
        assert_eq!(msg.channel(), Channel::Reliable);
        roundtrip(msg);
        let msg = Message::Call(crate::call::CallMsg {
            unreliable: true,
            ..call
        });
        assert_eq!(msg.channel(), Channel::Unreliable);
        roundtrip(msg);
        for ret in [
            crate::call::CallReturn {
                request: 42,
                ok: true,
                value: crate::call::encode_value(&crate::call::Arg::Bool(true)),
                error: String::new(),
            },
            crate::call::CallReturn {
                request: 43,
                ok: false,
                value: Vec::new(),
                error: "refused ✗".into(),
            },
        ] {
            let msg = Message::CallReturn(ret);
            assert_eq!(msg.channel(), Channel::Reliable);
            roundtrip(msg);
        }
        assert_eq!(channel_of_kind(kind::CALL), Channel::Reliable);
        assert_eq!(channel_of_kind(kind::CALL_RETURN), Channel::Reliable);
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
            key: String::new(),
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
                tick_rate: 1,
                key: String::new(),
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
        // D17: a WP0-WP14 peer (wire format 1) is named as such.
        let mut old = Protocol::new(60, 0);
        for it in old.items.iter_mut().filter(|i| i.name == "wire format") {
            it.value = "1".into();
        }
        assert_eq!(
            a.mismatch(&old.items, old.hash(), "server", "client")
                .as_deref(),
            Some(
                std::format!(
                    "protocol mismatch: wire format differs (server {WIRE_FORMAT}, client 1)"
                )
                .as_str()
            )
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
            Some("protocol mismatch: profiles is missing on the client")
        );
    }
}
