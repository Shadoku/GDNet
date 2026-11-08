//! Calls (docs/PLAN_NETCODE.md 1 rung 2, 2.1 "Auto-exposed methods", 2.4 `Call` / `CallReturn`, WP6).
//!
//! A call names an entity (net id), one of its parts (index), a method (a 32-bit FNV-1a hash of its
//! name: both peers derive the same method tables from the same scene and scripts, so no name travels),
//! a routing verb, opaque argument bytes (a postcard list of [`Arg`]s that gdnet makes from Variants),
//! an optional request id and the sender's tick. This module holds the wire types, the argument
//! encoding and the routing table; the session (session.rs, "Calls") runs it.
//!
//! Routing (one execution per peer, the authority first, so every peer sees the same result):
//!
//! | verb | from the server | from a client |
//! |---|---|---|
//! | `Sync` | runs locally, then goes to every joined peer that has the entity in scope | goes to the server only (the caller does not run it first); the server checks trust, runs it, then sends it to every peer in scope, the caller included |
//! | `Server` | runs locally | goes to the server, which runs it and answers with a `CallReturn` |
//! | `Owner` | runs locally when the server owns the entity, else goes to the owner | runs locally when the caller owns it; else via the server (which runs it when it owns the entity, else forwards it to the owner) |
//! | `Others` | goes to every joined peer | via the server, which runs it and forwards it to every peer but the caller |
//! | `Peer(id)` | runs locally for id 1, else goes to that peer | runs locally for its own id; else via the server (which runs it for id 1, else forwards it) |
//!
//! Every reliable call a client sends carries a request id: the server answers it with a `CallReturn`
//! (`ok` with the value for `Server`, `ok` with nothing for the others once routed, or an error: an
//! unknown method hash, a refused trust check, a destroyed entity, an unknown target peer). A request
//! not answered within the timeout (10 s) resolves as an error locally. Calls are reliable and ordered
//! on channel 0 by default, so a call is applied after the entity's `Create`; `unreliable` puts it on
//! channel 1 (no request id, may be lost; one that arrives before its entity is dropped).

use crate::entity::NetId;
use crate::peers::PeerId;
use serde::{Deserialize, Serialize};
use std::string::String;
use std::vec::Vec;

/// How long a request waits for its `CallReturn` before it resolves as an error.
pub const DEFAULT_CALL_TIMEOUT_US: u64 = 10_000_000;

/// FNV-1a 32 of a method name: the method's id on the wire.
pub fn method_hash(name: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in name.as_bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// FNV-1a 64 over one part's sorted method names (each terminated by a zero byte): what the `Scene`
/// action's property table carries per part, so peers whose method tables differ are caught at the
/// first spawn and the part is named.
pub fn method_table_hash(names: &[String]) -> u64 {
    let mut bytes = Vec::new();
    for n in names {
        bytes.extend_from_slice(n.as_bytes());
        bytes.push(0);
    }
    crate::entity::fnv1a64(&bytes)
}

/// The routing verbs (2.1, 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verb {
    /// The authority runs it, then every peer in scope.
    Sync,
    /// The server runs it and answers.
    Server,
    /// The entity's owner runs it.
    Owner,
    /// Everyone but the caller runs it.
    Others,
    /// One peer (`CallMsg::target`) runs it.
    Peer,
}

/// `Call` (message kind 11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMsg {
    pub entity: NetId,
    pub part: u16,
    pub method: u32,
    pub verb: Verb,
    /// The target of `Verb::Peer` (0 otherwise).
    pub target: PeerId,
    /// Who made the call. A client's own value is ignored: the server sets it from the transport.
    pub sender: PeerId,
    /// A request id the receiver answers with a `CallReturn` (0 = none).
    pub request: u32,
    /// The sender's tick when it made the call.
    pub tick: u64,
    /// Channel 1 instead of 0 (kept when the server relays it).
    pub unreliable: bool,
    /// A postcard list of [`Arg`]s.
    pub args: Vec<u8>,
}

/// `CallReturn` (message kind 12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallReturn {
    pub request: u32,
    pub ok: bool,
    /// A postcard [`Arg`] (the return value) when `ok`.
    pub value: Vec<u8>,
    /// Why it failed when not `ok`.
    pub error: String,
}

/// One argument (or return value) as it travels. gdnet makes them from Variants: the common scalars
/// directly, a `Resource` with a path as that path (loaded on receipt), anything else that holds no
/// object as Godot's own `var_to_bytes` encoding. Objects, Callables, Signals and RIDs never travel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Arg {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A resource by its path.
    Resource(String),
    /// `var_to_bytes` of a Variant that holds no object.
    Var(Vec<u8>),
}

/// Encodes an argument list.
pub fn encode_args(args: &[Arg]) -> Vec<u8> {
    postcard::to_allocvec(args).unwrap_or_default()
}

/// Decodes an argument list (None when malformed).
pub fn decode_args(bytes: &[u8]) -> Option<Vec<Arg>> {
    postcard::from_bytes(bytes).ok()
}

/// Encodes one value (a return value).
pub fn encode_value(v: &Arg) -> Vec<u8> {
    postcard::to_allocvec(v).unwrap_or_default()
}

/// Decodes one value; an empty buffer is `Nil`.
pub fn decode_value(bytes: &[u8]) -> Option<Arg> {
    if bytes.is_empty() {
        return Some(Arg::Nil);
    }
    postcard::from_bytes(bytes).ok()
}

/// A call this side makes ([`crate::Session::call`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutCall {
    pub entity: NetId,
    pub part: u16,
    pub method: u32,
    pub verb: Verb,
    pub target: PeerId,
    pub args: Vec<u8>,
    pub unreliable: bool,
}

/// What the session did with an [`OutCall`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallPlan {
    /// The caller runs the method now (it is the authority for this verb, or the target).
    pub run_local: bool,
    /// The request id a `CallReturn` will resolve (or a timeout).
    pub request: Option<u32>,
    /// Messages went out.
    pub sent: bool,
}

/// A call for gdnet to run (or check), delivered in order with the entity events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallIn {
    /// Server: the handle to report with `finish_call` / `refuse_call` (0 on a client: nothing to report).
    pub handle: u64,
    /// The manager key of the entity (for routing to its `NetManager`).
    pub manager: u64,
    pub entity: NetId,
    pub part: u16,
    pub method: u32,
    pub verb: Verb,
    pub sender: PeerId,
    pub tick: u64,
    pub args: Vec<u8>,
    /// Server: the sender does not own the entity, so the call needs `_net_allow` (D14).
    pub check: bool,
    /// Run the method here (false: the server only checks it and forwards it).
    pub run_local: bool,
}

/// A request resolved: by its `CallReturn`, or locally (a timeout, the session ending).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    pub request: u32,
    /// The encoded return value, or the error.
    pub result: Result<Vec<u8>, String>,
}

/// Call counters (the meter's source).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallStats {
    /// Call messages sent (relays included).
    pub sent: u64,
    /// Call messages received.
    pub received: u64,
    /// Calls the server refused (trust: `_net_allow` said no, or the sender does not own the entity).
    pub refused: u64,
    /// Calls dropped because their entity was destroyed before they arrived.
    pub dropped: u64,
    /// Calls naming a method hash the part does not have.
    pub unknown_method: u64,
    /// Requests that timed out.
    pub timeouts: u64,
    /// `CallReturn`s received.
    pub returns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_hash_is_fnv1a32() {
        assert_eq!(method_hash(""), 0x811c_9dc5);
        assert_eq!(method_hash("a"), 0xe40c_292c);
        assert_ne!(method_hash("open_door"), method_hash("open_doors"));
    }

    #[test]
    fn args_round_trip() {
        let args = std::vec![
            Arg::Nil,
            Arg::Bool(true),
            Arg::Int(-7),
            Arg::Float(1.5),
            Arg::Str(String::from("attack")),
            Arg::Resource(String::from("res://a.tres")),
            Arg::Var(std::vec![1, 2, 3]),
        ];
        let bytes = encode_args(&args);
        assert_eq!(decode_args(&bytes), Some(args));
        assert_eq!(decode_args(&[0xff, 0xff]), None);
        assert_eq!(decode_value(&[]), Some(Arg::Nil));
        assert_eq!(decode_value(&encode_value(&Arg::Int(3))), Some(Arg::Int(3)));
    }

    #[test]
    fn method_table_hash_depends_on_names() {
        let a = std::vec![String::from("open"), String::from("close")];
        let b = std::vec![String::from("open")];
        assert_ne!(method_table_hash(&a), method_table_hash(&b));
        assert_eq!(method_table_hash(&a), method_table_hash(&a.clone()));
    }
}

/// The routing matrix and the request plumbing, through the public `Session` API: a server and two
/// clients (transport ids 2 and 3) wired back to back, and a stand-in for gdnet that runs (or refuses)
/// what it is handed.
#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::entity::{CreateAction, EntityAction, ParentRef, MANAGER_ROOT};
    use crate::session::{EntityEvent, Session, SessionConfig, Spawn};
    use crate::wire::{self, Message};
    use crate::SERVER;

    const SCENE: &str = "res://door.tscn";
    const IDS: [PeerId; 2] = [2, 3];

    fn root() -> ParentRef {
        ParentRef {
            manager: 1,
            entity: MANAGER_ROOT,
            part: 0,
        }
    }

    struct Trio {
        s: Session,
        c: Vec<Session>,
        now: u64,
        /// Channels of the call messages that crossed the wire, in order.
        call_channels: Vec<u8>,
    }

    /// What one side's stand-in for gdnet ran: (verb, sender, method).
    type Ran = Vec<(Verb, PeerId, u32)>;

    impl Trio {
        fn new() -> Self {
            let mut t = Self {
                s: Session::server(SessionConfig::new(60)),
                c: std::vec![
                    Session::client(SessionConfig::new(60)),
                    Session::client(SessionConfig::new(60)),
                ],
                now: 1_000_000,
                call_channels: Vec::new(),
            };
            for (i, id) in IDS.iter().enumerate() {
                t.s.peer_connected(*id, t.now);
                t.c[i].peer_connected(SERVER, t.now);
            }
            t.pump();
            assert!(t.c.iter().all(Session::is_connected));
            t.s.register_scene(SCENE, 1);
            t
        }

        fn pump(&mut self) {
            for _ in 0..3 {
                self.now += 16_667;
                self.s.tick(self.now);
                for c in &mut self.c {
                    c.tick(self.now);
                }
                for (i, c) in self.c.iter_mut().enumerate() {
                    for o in c.outgoing() {
                        if o.bytes.first() == Some(&wire::kind::CALL) {
                            self.call_channels.push(o.channel);
                        }
                        self.s.receive(IDS[i], &o.bytes, self.now);
                    }
                }
                for o in self.s.outgoing() {
                    if o.bytes.first() == Some(&wire::kind::CALL) {
                        self.call_channels.push(o.channel);
                    }
                    let i = IDS.iter().position(|p| *p == o.peer).expect("a client");
                    self.c[i].receive(SERVER, &o.bytes, self.now);
                }
            }
        }

        fn spawn(&mut self, owner: PeerId) -> NetId {
            let id = self
                .s
                .spawn(
                    SCENE,
                    Spawn::new(root(), "Door", owner, 2, Vec::new()),
                    self.now,
                )
                .unwrap();
            self.pump();
            self.drain_all();
            id
        }

        fn drain_all(&mut self) {
            while self.s.poll_entity_event().is_some() {}
            for c in &mut self.c {
                while c.poll_entity_event().is_some() {}
            }
        }

        /// The server's stand-in: every call event is run (or refused when it needs `_net_allow` and
        /// `allow` is false) and finished with the return value 99. Returns what ran on the server and
        /// the events' (check, run_local) flags.
        fn serve(&mut self, allow: bool) -> (Ran, Vec<(bool, bool)>) {
            let (mut ran, mut flags) = (Vec::new(), Vec::new());
            while let Some(e) = self.s.poll_entity_event() {
                if let EntityEvent::Call(c) = e {
                    flags.push((c.check, c.run_local));
                    if c.check && !allow {
                        self.s
                            .refuse_call(c.handle, "refused by _net_allow", self.now);
                        continue;
                    }
                    if c.run_local {
                        ran.push((c.verb, c.sender, c.method));
                    }
                    self.s
                        .finish_call(c.handle, Ok(encode_value(&Arg::Int(99))), self.now);
                }
            }
            (ran, flags)
        }

        /// What client `i` was handed to run.
        fn ran(&mut self, i: usize) -> Ran {
            let mut out = Vec::new();
            while let Some(e) = self.c[i].poll_entity_event() {
                if let EntityEvent::Call(c) = e {
                    assert_eq!(c.handle, 0, "a client reports nothing back");
                    out.push((c.verb, c.sender, c.method));
                }
            }
            out
        }

        fn results(&mut self, i: usize) -> Vec<CallResult> {
            let mut out = Vec::new();
            while let Some(r) = self.c[i].poll_call_result() {
                out.push(r);
            }
            out
        }
    }

    fn out(entity: NetId, verb: Verb, target: PeerId) -> OutCall {
        OutCall {
            entity,
            part: 0,
            method: method_hash("open_door"),
            verb,
            target,
            args: encode_args(&[Arg::Int(7)]),
            unreliable: false,
        }
    }

    const M: fn() -> u32 = || method_hash("open_door");

    #[test]
    fn server_calls_route_by_verb() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        let cases: [(Verb, PeerId, bool, [bool; 2]); 6] = [
            (Verb::Sync, 0, true, [true, true]),
            (Verb::Server, 0, true, [false, false]),
            (Verb::Owner, 0, false, [true, false]),
            (Verb::Others, 0, false, [true, true]),
            (Verb::Peer, 3, false, [false, true]),
            (Verb::Peer, SERVER, true, [false, false]),
        ];
        for (verb, target, local, reach) in cases {
            let plan = t.s.call(out(id, verb, target), t.now).unwrap();
            assert_eq!(plan.run_local, local, "{verb:?} {target}");
            assert_eq!(plan.request, None, "the server waits for nothing");
            t.pump();
            for (i, r) in reach.iter().enumerate() {
                let ran = t.ran(i);
                let want = if *r {
                    std::vec![(verb, SERVER, M())]
                } else {
                    Vec::new()
                };
                assert_eq!(ran, want, "{verb:?} {target} at client {}", IDS[i]);
            }
        }
        assert!(
            t.s.call(out(id, Verb::Peer, 9), t.now).is_err(),
            "an unknown peer"
        );
        assert!(
            t.s.call(out(999, Verb::Sync, 0), t.now).is_err(),
            "an unknown entity"
        );
    }

    #[test]
    fn client_calls_go_through_the_server() {
        let mut t = Trio::new();
        let own = t.spawn(2);
        let theirs = t.spawn(3);
        // (entity, verb, target, runs on the caller, runs on the server, reaches [client 2, client 3])
        let cases: [(NetId, Verb, PeerId, bool, bool, [bool; 2]); 9] = [
            (own, Verb::Sync, 0, false, true, [true, true]),
            (own, Verb::Server, 0, false, true, [false, false]),
            (own, Verb::Owner, 0, true, false, [false, false]),
            (theirs, Verb::Owner, 0, false, false, [false, true]),
            (own, Verb::Others, 0, false, true, [false, true]),
            (own, Verb::Peer, 3, false, false, [false, true]),
            (own, Verb::Peer, SERVER, false, true, [false, false]),
            (own, Verb::Peer, 2, true, false, [false, false]),
            (theirs, Verb::Sync, 0, false, true, [true, true]),
        ];
        for (entity, verb, target, local, on_server, reach) in cases {
            let plan = t.c[0].call(out(entity, verb, target), t.now).unwrap();
            assert_eq!(plan.run_local, local, "{verb:?} {target}");
            assert_eq!(
                plan.request.is_some(),
                !local,
                "a sent call waits for its answer"
            );
            t.pump();
            let (ran, flags) = t.serve(true);
            let want_check = entity == theirs;
            assert!(
                flags.iter().all(|f| f.0 == want_check),
                "{verb:?}: the trust check is needed only for an entity the sender does not own"
            );
            assert_eq!(
                !ran.is_empty(),
                on_server,
                "{verb:?} {target} on the server"
            );
            for r in &ran {
                assert_eq!(r.1, 2, "the server knows the sender");
            }
            t.pump();
            for (i, r) in reach.iter().enumerate() {
                let ran = t.ran(i);
                let want = if *r {
                    std::vec![(verb, 2, M())]
                } else {
                    Vec::new()
                };
                assert_eq!(ran, want, "{verb:?} {target} at client {}", IDS[i]);
            }
            let results = t.results(0);
            if local {
                assert!(results.is_empty());
            } else {
                assert_eq!(results.len(), 1, "{verb:?}");
                assert_eq!(Some(results[0].request), plan.request);
                let value = if verb == Verb::Server {
                    encode_value(&Arg::Int(99))
                } else {
                    Vec::new()
                };
                assert_eq!(results[0].result, Ok(value), "{verb:?}");
            }
        }
    }

    #[test]
    fn a_non_owner_sync_is_refused_and_the_caller_told() {
        let mut t = Trio::new();
        let theirs = t.spawn(3);
        let plan = t.c[0].call(out(theirs, Verb::Sync, 0), t.now).unwrap();
        t.pump();
        let (ran, flags) = t.serve(false);
        assert_eq!(flags, std::vec![(true, true)]);
        assert!(ran.is_empty());
        t.pump();
        assert!(t.ran(0).is_empty() && t.ran(1).is_empty(), "nobody ran it");
        let r = t.results(0);
        assert_eq!(r.len(), 1);
        assert_eq!(Some(r[0].request), plan.request);
        assert_eq!(r[0].result, Err(String::from("refused by _net_allow")));
        assert_eq!(t.s.call_stats().refused, 1);
    }

    #[test]
    fn an_unknown_method_hash_is_an_error_for_the_caller() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        t.s.register_methods(
            crate::entity::scene_hash(SCENE),
            std::vec![std::vec![M()], Vec::new()],
        );
        let mut bad = out(id, Verb::Sync, 0);
        bad.method = method_hash("_secret");
        t.c[0].call(bad, t.now).unwrap();
        let mut wrong_part = out(id, Verb::Server, 0);
        wrong_part.part = 1;
        t.c[0].call(wrong_part, t.now).unwrap();
        t.c[0].call(out(id, Verb::Server, 0), t.now).unwrap();
        t.pump();
        let (ran, _) = t.serve(true);
        assert_eq!(ran.len(), 1, "only the known method reached gdnet");
        t.pump();
        let r = t.results(0);
        assert_eq!(r.len(), 3);
        for bad in &r[..2] {
            let e = bad.result.clone().unwrap_err();
            assert!(e.contains("unknown method") && e.contains(SCENE), "{e}");
        }
        assert!(r[2].result.is_ok());
        assert_eq!(t.s.call_stats().unknown_method, 2);
        assert!(t.c[0].is_connected(), "not a disconnect");
    }

    #[test]
    fn returns_are_matched_to_their_requests() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        let a = t.c[0].call(out(id, Verb::Server, 0), t.now).unwrap();
        let b = t.c[0].call(out(id, Verb::Server, 0), t.now).unwrap();
        assert_ne!(a.request, b.request);
        t.pump();
        let mut handles = Vec::new();
        while let Some(e) = t.s.poll_entity_event() {
            if let EntityEvent::Call(c) = e {
                handles.push(c.handle);
            }
        }
        // Answered in reverse order with different values.
        t.s.finish_call(handles[1], Ok(encode_value(&Arg::Str("b".into()))), t.now);
        t.s.finish_call(handles[0], Ok(encode_value(&Arg::Str("a".into()))), t.now);
        t.pump();
        let r = t.results(0);
        let by = |req: Option<u32>| r.iter().find(|x| Some(x.request) == req).unwrap();
        assert_eq!(
            by(a.request).result,
            Ok(encode_value(&Arg::Str("a".into())))
        );
        assert_eq!(
            by(b.request).result,
            Ok(encode_value(&Arg::Str("b".into())))
        );
        assert_eq!(t.c[0].pending_requests(), 0);
    }

    #[test]
    fn a_call_waits_behind_its_pending_create() {
        let mut t = Trio::new();
        let c = &mut t.c[0];
        let now = t.now;
        let feed = |c: &mut Session, m: Message| c.receive(SERVER, &wire::encode(&m), now);
        feed(
            c,
            Message::Entity(EntityAction::Scene {
                hash: crate::entity::scene_hash(SCENE),
                path: SCENE.into(),
                parts_hash: 1,
                props: Default::default(),
            }),
        );
        let create = |id: NetId, parent: NetId| {
            Message::Entity(EntityAction::Create(CreateAction {
                id,
                scene_hash: crate::entity::scene_hash(SCENE),
                parent: ParentRef {
                    manager: 1,
                    entity: parent,
                    part: 0,
                },
                name: std::format!("E{id}"),
                owner: 1,
                parts: 2,
                initial_state: Vec::new(),
            }))
        };
        let call = |id: NetId| {
            Message::Call(CallMsg {
                entity: id,
                part: 0,
                method: M(),
                verb: Verb::Sync,
                target: 0,
                sender: SERVER,
                request: 0,
                tick: 5,
                unreliable: false,
                args: Vec::new(),
            })
        };
        // The child's create (its parent unknown) and a call on it, then the parent.
        feed(c, create(11, 10));
        feed(c, call(11));
        assert!(c.poll_entity_event().is_none(), "held");
        feed(c, create(10, MANAGER_ROOT));
        let mut order = Vec::new();
        while let Some(e) = c.poll_entity_event() {
            order.push(match e {
                EntityEvent::Create { create, .. } => std::format!("create {}", create.id),
                EntityEvent::Call(x) => std::format!("call {}", x.entity),
                other => std::format!("{other:?}"),
            });
        }
        assert_eq!(order, ["create 10", "create 11", "call 11"]);
        assert_eq!(c.held_len(), 0);
        assert_eq!(c.call_stats().dropped, 0);
    }

    #[test]
    fn a_call_for_a_destroyed_entity_is_dropped() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        // Server to client: the destroy goes first, so the call finds nothing (silently).
        t.s.destroy(id, t.now);
        let raw = Message::Call(CallMsg {
            entity: id,
            part: 0,
            method: M(),
            verb: Verb::Sync,
            target: 0,
            sender: SERVER,
            request: 0,
            tick: 1,
            unreliable: false,
            args: Vec::new(),
        });
        t.pump();
        t.c[1].receive(SERVER, &wire::encode(&raw), t.now);
        assert!(t.ran(1).is_empty());
        assert_eq!(t.c[1].call_stats().dropped, 1);
        // Client to server: the entity goes while the call is on its way; the caller hears why.
        let id = t.spawn(2);
        t.c[0].call(out(id, Verb::Sync, 0), t.now).unwrap();
        t.s.destroy(id, t.now);
        t.pump();
        assert!(t.serve(true).0.is_empty());
        t.pump();
        let r = t.results(0);
        assert!(
            r[0].result.clone().unwrap_err().contains("destroyed"),
            "{r:?}"
        );
        assert_eq!(t.s.call_stats().dropped, 1);
    }

    #[test]
    fn an_unanswered_request_times_out() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        t.c[0].set_call_timeout_us(100_000);
        let plan = t.c[0].call(out(id, Verb::Server, 0), t.now).unwrap();
        t.pump(); // the server holds it (nobody serves it)
        assert!(t.results(0).is_empty());
        for _ in 0..3 {
            t.pump();
        }
        let r = t.results(0);
        assert_eq!(r.len(), 1);
        assert_eq!(Some(r[0].request), plan.request);
        assert!(r[0].result.clone().unwrap_err().contains("timed out"));
        assert_eq!(t.c[0].call_stats().timeouts, 1);
        // A late answer is ignored.
        t.serve(true);
        t.pump();
        assert!(t.results(0).is_empty());
    }

    #[test]
    fn channels_follow_the_unreliable_flag() {
        let mut t = Trio::new();
        let id = t.spawn(2);
        t.call_channels.clear();
        let plan = t.c[0].call(out(id, Verb::Sync, 0), t.now).unwrap();
        assert!(plan.request.is_some());
        let mut fast = out(id, Verb::Sync, 0);
        fast.unreliable = true;
        let plan = t.c[0].call(fast, t.now).unwrap();
        assert_eq!(plan.request, None, "an unreliable call waits for nothing");
        t.pump();
        t.serve(true);
        t.pump();
        // Client to server: 0 then 1; the relays keep their channels (two clients each).
        assert_eq!(t.call_channels, [0, 1, 0, 0, 1, 1]);
        let mut idle = Session::client(SessionConfig::new(60));
        assert!(
            idle.call(out(id, Verb::Sync, 0), 0).is_err(),
            "not connected"
        );
    }
}
