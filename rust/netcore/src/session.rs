//! The session (docs/PLAN_NETCODE.md 2.2 steps 1 and 6, 2.4, WP1): a sans-IO state machine fed with the
//! transport's events and packets, producing packets to send and events for the game.
//!
//! ```text
//! transport            Session                     game
//! peer_connected  ---> peer_connected(peer, now)
//! packet          ---> receive(peer, bytes, now)
//! physics tick    ---> tick(now)
//!                      outgoing()  ---> send_bytes(peer, channel, reliability, bytes)
//!                      poll_event() --> PeerJoined / PeerLeft / Connected / Disconnected / DisconnectPeer
//! ```
//!
//! Handshake: when the transport connects a client to the server, the client sends `Hello` (protocol
//! hash and items, tick rate). The server answers `Welcome` (its tick, the client's id and id range) or
//! `Reject` (a reason naming the first differing protocol item). On `Reject` the client reports
//! `Disconnected(reason)` and the game closes the transport; the server also asks for the peer to be
//! disconnected one second later, in case the client does not.
//!
//! `now` is microseconds since any epoch, supplied by the caller.

use crate::authority::{self, AuthorityStats, Decision, StateMsg, Transfer, WriteReject};
use crate::call::{
    CallIn, CallMsg, CallPlan, CallResult, CallReturn, CallStats, OutCall, Verb,
    DEFAULT_CALL_TIMEOUT_US,
};
use crate::clock::TickClock;
use crate::delta::{ReplStats, Replication, StateUpdate};
use crate::emul::{EmulSettings, Emulator};
use crate::entity::{
    CreateAction, EntityAction, EntityRecord, EntityTable, HeldQueue, IdAllocator, NetId, ParentRef,
};
use crate::hash::{HashMsg, HashRx, HashStats, HashTx};
use crate::input::{self, AdaptiveDelay, InputBuffer, InputFrame, OwnerInputs, TakenFrame};
use crate::interp::Interp;
use crate::keyed::{KeyEvent, KeyMsg, KeyedState, Who};
use crate::peers::{PeerId, PeerTable, Stage, SERVER};
use crate::predict::{self, Policy, PredictStats, Prediction, Rollback};
use crate::props::{PropDesc, ScenePropsTable, Value};
use crate::wire::{
    self, Hello, IdRange, Message, Ping, Pong, Protocol, Reject, Reliability, Welcome,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::string::String;
use std::vec::Vec;

/// Entity ids each peer may allocate: slot `n` owns `[n * ID_RANGE_LEN, (n + 1) * ID_RANGE_LEN)`; the
/// server is slot 0 (without id 0, which means "the manager root"), each joining client gets the next.
pub const ID_RANGE_LEN: u32 = 1 << 16;

/// At most this many entity actions wait for an entity they name; one more ends the session (on a
/// client) or drops the peer that sent it (on the server), with a readable reason.
pub const MAX_HELD: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub peer: PeerId,
    pub channel: u8,
    pub reliability: Reliability,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A peer finished the handshake (on the server: a client; on a client: the server, id 1).
    PeerJoined(PeerId),
    /// A joined peer left.
    PeerLeft(PeerId),
    /// This side is in the session: a server at its first tick, a client at `Welcome`.
    Connected { local_id: PeerId, tick: u64 },
    /// This side left the session, with a readable reason.
    Disconnected(String),
    /// The transport should drop this peer (a rejected handshake).
    DisconnectPeer { peer: PeerId, reason: String },
    /// A readable entity-layer problem that does not end the session (an action held too long).
    EntityError(String),
}

/// What remote entity actions did to this peer's table, in order, for gdnet to apply to nodes.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityEvent {
    /// Instantiate `scene_path` under `create.parent`. On the server, `proposal` marks a client's create
    /// (spawn policy `owner_allowed`) that gdnet must accept (`accept_create`) or refuse
    /// (`refuse_create`); it is not in the table until accepted.
    Create {
        create: CreateAction,
        scene_path: String,
        from: PeerId,
        proposal: bool,
    },
    /// Free this entity. `cascade` marks entities removed because an ancestor was destroyed; they come
    /// before the ancestor, which comes last with `cascade == false`.
    Destroy { record: EntityRecord, cascade: bool },
    /// Move the entity under `parent` (`from_manager` is the manager it was under).
    Reparent {
        id: NetId,
        from_manager: u64,
        parent: ParentRef,
    },
    SetOwner {
        id: NetId,
        manager: u64,
        owner: PeerId,
    },
    /// WP6: a call to run (or, on the server, to check and route), in order with the entity events.
    Call(CallIn),
    /// WP7: this peer owns (or, on the server, holds for the server) a `request` entity that
    /// `requester` asks for; gdnet answers with [`Session::answer_owner_query`].
    OwnerQuery {
        id: NetId,
        manager: u64,
        requester: PeerId,
    },
    /// WP7: the outcome of this peer's ownership request.
    OwnerReply {
        id: NetId,
        manager: u64,
        granted: bool,
    },
    /// WP7: a rejected write. On the server: `peer` wrote fields it may not write. On the writer
    /// (`peer` = itself): restore `restore` (the server's last values).
    WriteRejected {
        id: NetId,
        manager: u64,
        peer: PeerId,
        restore: Vec<(u16, u16, Value)>,
    },
}

impl EntityEvent {
    /// The manager key the event belongs to.
    pub fn manager(&self) -> u64 {
        match self {
            EntityEvent::Create { create, .. } => create.parent.manager,
            EntityEvent::Destroy { record, .. } => record.parent.manager,
            EntityEvent::Reparent { from_manager, .. } => *from_manager,
            EntityEvent::SetOwner { manager, .. }
            | EntityEvent::OwnerQuery { manager, .. }
            | EntityEvent::OwnerReply { manager, .. }
            | EntityEvent::WriteRejected { manager, .. } => *manager,
            EntityEvent::Call(c) => c.manager,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionConfig {
    /// WP3 (3.3): a hash of the effective profiles, part of the `Hello` protocol items.
    pub profiles_hash: u64,
    /// WP3 (2.7): snapshot bytes per second per peer (0 = 32,768).
    pub budget_bytes_per_s: u32,
    /// Ticks per second (Godot's physics ticks per second).
    pub tick_rate: u32,
    /// Debug hook: any value other than 0 changes the protocol hash, forcing a mismatch.
    pub protocol_salt: u64,
    /// A `Time` ping goes to each joined peer every this many ticks (0 = a quarter second).
    pub ping_interval_ticks: u32,
    /// An entity action waiting this many ticks for an entity it names is reported as an
    /// `EntityError` (it keeps waiting). 0 = five seconds.
    pub held_timeout_ticks: u32,
    /// WP4: ticks of prediction history per owned entity (D8: 20).
    pub max_rollback_ticks: u32,
}

impl SessionConfig {
    pub fn new(tick_rate: u32) -> Self {
        Self {
            profiles_hash: 0,
            budget_bytes_per_s: 0,
            tick_rate,
            protocol_salt: 0,
            ping_interval_ticks: 0,
            held_timeout_ticks: 0,
            max_rollback_ticks: predict::DEFAULT_MAX_ROLLBACK,
        }
    }
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets_in: u64,
    pub packets_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub decode_errors: u64,
    /// Entity actions dropped because the sender was not allowed to send them.
    pub entity_refused: u64,
}

pub struct Session {
    role: Role,
    local_id: PeerId,
    protocol: Protocol,
    clock: TickClock,
    peers: PeerTable,
    out: Vec<Outgoing>,
    events: VecDeque<Event>,
    ping_interval: u64,
    next_slot: u32,
    connected: bool,
    closed: bool,
    started: bool,
    hello_sent_us: Option<u64>,
    id_range: IdRange,
    emul_out: Option<Emulator<Outgoing>>,
    emul_in: Option<Emulator<(PeerId, Vec<u8>)>>,
    stats: Stats,
    entities: EntityTable,
    ids: IdAllocator,
    held: HeldQueue,
    held_timeout: u64,
    /// Server: client creates waiting for gdnet's verdict, with their sender.
    proposals: BTreeMap<NetId, (CreateAction, PeerId)>,
    entity_events: VecDeque<EntityEvent>,
    /// Scene hash to path, from local registrations and remote announcements.
    scene_paths: BTreeMap<u64, String>,
    /// Part table hashes peers announced, by (peer, scene hash).
    remote_scenes: BTreeMap<(PeerId, u64), u64>,
    /// Scenes already announced to each peer.
    scenes_sent: BTreeMap<PeerId, BTreeSet<u64>>,
    /// WP3: state replication (property tables, recorded values, per-peer baselines).
    repl: Replication,
    budget_bytes_per_s: u32,
    /// WP3: property tables peers announced, by (peer, scene hash).
    remote_props: BTreeMap<(PeerId, u64), ScenePropsTable>,
    /// WP3: part names (paths) per scene hash, for mismatch reasons.
    part_names: BTreeMap<u64, Vec<String>>,
    /// WP4: received state for gdnet (snapshot values minus the hot fields of predicted entities).
    pass_updates: VecDeque<StateUpdate>,
    /// WP4 owner side: prediction histories and the confirmed mirror.
    prediction: Prediction,
    /// WP4 owner side: the frames this peer sampled, by entity and tick.
    owner_inputs: OwnerInputs,
    /// WP4 server side: frames received per client-owned entity.
    input_buffers: BTreeMap<NetId, InputBuffer>,
    /// WP4 server side: the frame each client-owned entity uses this tick.
    server_inputs: BTreeMap<NetId, TakenFrame>,
    /// WP4: the client's prediction lead in ticks (None until the round trip is known).
    lead: Option<i64>,
    input_delay: u32,
    adaptive: Option<AdaptiveDelay>,
    interaction: bool,
    input_stats: InputStats,
    /// WP5 client side: the render timeline, replicas' sample buffers, error and owned offsets.
    interp: Interp,
    /// WP6: requests, inbound calls awaiting gdnet's verdict, method tables, counters.
    calls: CallState,
    /// WP7: ownership requests, owner state, rejected writes.
    auth: AuthState,
    /// WP7: the keyed state map.
    keyed: KeyedState,
    /// WP8: hash telemetry (hash.rs).
    hash: HashState,
}

/// WP8: the session's hash telemetry state.
struct HashState {
    /// `gdnet/debug/hash_telemetry` (default on).
    on: bool,
    /// Entities sampled per tick (server).
    sample: u32,
    tx: HashTx,
    rx: HashRx,
    sent: u64,
    /// Server: per (peer, entity) the hash last sent and the tick it was sent; an unchanged hash is
    /// only sent again after `refresh_ticks` (an idle world costs almost nothing, gate G4).
    last: BTreeMap<(PeerId, NetId), (u32, u64)>,
    /// 0 = `REFRESH_S` seconds.
    refresh_ticks: u64,
}

impl Default for HashState {
    fn default() -> Self {
        Self {
            on: true,
            sample: crate::hash::DEFAULT_SAMPLE,
            tx: HashTx::default(),
            rx: HashRx::default(),
            sent: 0,
            last: BTreeMap::new(),
            refresh_ticks: 0,
        }
    }
}

/// WP7: the session's authority state (authority.rs).
#[derive(Default)]
struct AuthState {
    stats: AuthorityStats,
    /// Server: ownership queries waiting for an answer, (entity, requester) -> the tick asked.
    queries: BTreeMap<(NetId, PeerId), u64>,
    /// Server: the newest owner-state tick applied per `client_owned` entity.
    owner_tick: BTreeMap<NetId, u64>,
    /// Server: the tick of the last reject of an owner state, per (peer, entity).
    reject_tick: BTreeMap<(PeerId, NetId), u64>,
    /// Client: the last value the server sent for each field (what a rejected write restores).
    received: BTreeMap<(NetId, u16, u16), Value>,
    /// Client: fields written and sent, waiting for the reject (or a newer value from the server).
    pending_writes: BTreeSet<(NetId, u16, u16)>,
    /// Client: write entries to send at the next post-tick.
    write_queue: Vec<(NetId, Vec<u8>)>,
    /// Client: rejects in a row per field without a new value from the server; at
    /// [`MAX_REJECTS_IN_A_ROW`] the field is no longer checked (the restore does not stick: code keeps
    /// writing it, or its value does not survive a write and read back).
    rejects: BTreeMap<(NetId, u16, u16), u32>,
    /// Client: the tick each owned entity's state was last sent.
    last_sent: BTreeMap<NetId, u64>,
}

/// An ownership query waiting this long (seconds) is refused.
pub const OWNER_QUERY_TIMEOUT_S: u64 = 5;

/// A field rejected this many times in a row is no longer checked for writes (until the server sends a
/// new value for it).
pub const MAX_REJECTS_IN_A_ROW: u32 = 3;

/// WP6: the session's call state.
#[derive(Default)]
struct CallState {
    next_request: u32,
    next_handle: u64,
    /// Requests awaiting their `CallReturn`, with the deadline (microseconds).
    pending: BTreeMap<u32, u64>,
    /// Server: received calls gdnet has not finished yet, with their sender.
    inbound: BTreeMap<u64, (CallMsg, PeerId)>,
    results: VecDeque<CallResult>,
    /// Per scene hash, per part: the sorted method hashes (local only; the wire carries their hash).
    methods: BTreeMap<u64, Vec<Vec<u32>>>,
    stats: CallStats,
    timeout_us: u64,
}

/// WP4 input counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputStats {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub frames_received: u64,
    /// Ticks the server repeated a client's last frame because the frame for the tick was missing.
    pub repeated: u64,
    /// Frames dropped because the sender does not own the entity.
    pub refused: u64,
}

/// The default snapshot budget per peer (2.7, D8).
pub const DEFAULT_BUDGET: u32 = 32_768;

impl Session {
    fn new(role: Role, cfg: SessionConfig) -> Self {
        let rate = cfg.tick_rate.max(1);
        let ping = if cfg.ping_interval_ticks == 0 {
            (rate / 4).max(1)
        } else {
            cfg.ping_interval_ticks
        };
        Self {
            role,
            local_id: if role == Role::Server { SERVER } else { 0 },
            protocol: Protocol::with_profiles(rate, cfg.protocol_salt, cfg.profiles_hash),
            clock: TickClock::new(rate),
            peers: PeerTable::default(),
            out: Vec::new(),
            events: VecDeque::new(),
            ping_interval: u64::from(ping),
            next_slot: 1,
            connected: false,
            closed: false,
            started: false,
            hello_sent_us: None,
            id_range: if role == Role::Server {
                IdRange {
                    start: 0,
                    len: ID_RANGE_LEN,
                }
            } else {
                IdRange::default()
            },
            emul_out: None,
            emul_in: None,
            stats: Stats::default(),
            entities: EntityTable::default(),
            ids: if role == Role::Server {
                IdAllocator::new(IdRange {
                    start: 0,
                    len: ID_RANGE_LEN,
                })
            } else {
                IdAllocator::default()
            },
            held: HeldQueue::default(),
            held_timeout: u64::from(if cfg.held_timeout_ticks == 0 {
                rate.saturating_mul(5)
            } else {
                cfg.held_timeout_ticks
            }),
            proposals: BTreeMap::new(),
            entity_events: VecDeque::new(),
            scene_paths: BTreeMap::new(),
            remote_scenes: BTreeMap::new(),
            scenes_sent: BTreeMap::new(),
            repl: Replication::default(),
            budget_bytes_per_s: if cfg.budget_bytes_per_s == 0 {
                DEFAULT_BUDGET
            } else {
                cfg.budget_bytes_per_s
            },
            remote_props: BTreeMap::new(),
            part_names: BTreeMap::new(),
            pass_updates: VecDeque::new(),
            prediction: Prediction::new(cfg.max_rollback_ticks),
            owner_inputs: OwnerInputs::new(u64::from(cfg.max_rollback_ticks) + 16),
            input_buffers: BTreeMap::new(),
            server_inputs: BTreeMap::new(),
            lead: None,
            input_delay: 0,
            adaptive: None,
            interaction: false,
            input_stats: InputStats::default(),
            interp: Interp::new(rate),
            calls: CallState {
                timeout_us: DEFAULT_CALL_TIMEOUT_US,
                ..CallState::default()
            },
            auth: AuthState::default(),
            keyed: KeyedState::default(),
            hash: HashState::default(),
        }
    }

    /// The hosting side (peer id 1). It reports `Connected` at its first tick.
    pub fn server(cfg: SessionConfig) -> Self {
        Self::new(Role::Server, cfg)
    }

    /// A joining side. Its id is the one the server's `Welcome` names.
    pub fn client(cfg: SessionConfig) -> Self {
        Self::new(Role::Client, cfg)
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// This side's peer id (1 on the server, 0 on a client until `Welcome`).
    pub fn local_id(&self) -> PeerId {
        self.local_id
    }

    pub fn protocol(&self) -> &Protocol {
        &self.protocol
    }

    pub fn tick_rate(&self) -> u32 {
        self.clock.tick_rate()
    }

    /// The local tick (a client adopts the server's at `Welcome`, then counts its own).
    pub fn current_tick(&self) -> u64 {
        self.clock.tick()
    }

    pub fn clock(&self) -> &TickClock {
        &self.clock
    }

    /// True while in the session: the server from its first tick, a client from `Welcome`.
    pub fn is_connected(&self) -> bool {
        self.connected && !self.closed
    }

    /// True once the session ended (`Disconnected` was reported).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn peers(&self) -> &PeerTable {
        &self.peers
    }

    /// Remote peers that finished the handshake, ascending.
    pub fn joined_peers(&self) -> Vec<PeerId> {
        self.peers.joined()
    }

    /// Smoothed RTT to `peer` in milliseconds, `None` before the first sample or for an unknown peer.
    pub fn rtt_ms(&self, peer: PeerId) -> Option<f64> {
        self.peers.get(peer).and_then(|p| p.rtt.rtt_ms())
    }

    pub fn jitter_ms(&self, peer: PeerId) -> Option<f64> {
        self.peers.get(peer).and_then(|p| p.rtt.jitter_ms())
    }

    /// This side's entity id range (the server's is slot 0; a client's comes in `Welcome`).
    pub fn id_range(&self) -> IdRange {
        self.id_range
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Emulates this side's traffic: `send` on what it sends, `receive` on what it receives. Off settings
    /// remove the emulator. Used when the transport cannot emulate (any peer but gdnet's pipe).
    pub fn set_emulation(&mut self, send: EmulSettings, receive: EmulSettings, seed: u64) {
        if send.is_off() {
            self.emul_out = None;
        } else if let Some(e) = self.emul_out.as_mut() {
            e.set_settings(send);
        } else {
            self.emul_out = Some(Emulator::new(send, seed));
        }
        if receive.is_off() {
            self.emul_in = None;
        } else if let Some(e) = self.emul_in.as_mut() {
            e.set_settings(receive);
        } else {
            self.emul_in = Some(Emulator::new(receive, seed.wrapping_add(1)));
        }
    }

    fn send(&mut self, peer: PeerId, msg: &Message, now_us: u64) {
        let channel = msg.channel();
        let packet = Outgoing {
            peer,
            channel: channel.index(),
            reliability: channel.reliability(),
            bytes: wire::encode(msg),
        };
        match self.emul_out.as_mut() {
            Some(e) => {
                let ordered =
                    (channel.reliability() == Reliability::Reliable).then_some(channel.index());
                e.push(now_us, packet, ordered);
            }
            None => self.queue_out(packet),
        }
    }

    fn queue_out(&mut self, packet: Outgoing) {
        self.stats.packets_out += 1;
        self.stats.bytes_out += packet.bytes.len() as u64;
        self.out.push(packet);
    }

    fn ping(&mut self, peer: PeerId, now_us: u64) {
        let tick = self.clock.tick();
        self.send(
            peer,
            &Message::Ping(Ping {
                sent_us: now_us,
                tick,
            }),
            now_us,
        );
    }

    /// The transport connected `peer`. A client starts the handshake when the server (id 1) connects.
    pub fn peer_connected(&mut self, peer: PeerId, now_us: u64) {
        if self.closed {
            return;
        }
        match self.role {
            Role::Server => {
                self.peers.insert(peer, self.clock.tick());
            }
            Role::Client => {
                if peer != SERVER || !self.peers.insert(peer, self.clock.tick()) {
                    return;
                }
                let hello = Hello {
                    protocol_hash: self.protocol.hash(),
                    items: self.protocol.items.clone(),
                    tick_rate: self.tick_rate(),
                };
                self.hello_sent_us = Some(now_us);
                self.send(SERVER, &Message::Hello(hello), now_us);
            }
        }
    }

    /// The transport lost `peer`.
    pub fn peer_disconnected(&mut self, peer: PeerId, now_us: u64) {
        if self.role == Role::Server && self.peers.is_joined(peer) {
            // WP7: a departed client's entities stay, owned by the server again.
            let owned: Vec<NetId> = self
                .entities
                .iter()
                .filter(|r| r.owner == peer)
                .map(|r| r.id)
                .collect();
            for id in owned {
                self.set_owner(id, SERVER, now_us);
            }
            let stale: Vec<(NetId, PeerId)> = self
                .auth
                .queries
                .keys()
                .filter(|(id, r)| {
                    *r == peer || self.entities.get(*id).is_some_and(|e| e.owner == peer)
                })
                .copied()
                .collect();
            for (id, r) in stale {
                self.finish_query(id, r, false, now_us);
            }
            self.keyed.forget_peer(peer);
            self.auth.reject_tick.retain(|(p, _), _| *p != peer);
        }
        self.scenes_sent.remove(&peer);
        self.repl.remove_peer(peer);
        self.remote_props.retain(|(p, _), _| *p != peer);
        self.remote_scenes.retain(|(p, _), _| *p != peer);
        let Some(state) = self.peers.remove(peer) else {
            return;
        };
        if state.stage == Stage::Joined {
            self.events.push_back(Event::PeerLeft(peer));
        }
        if self.role == Role::Client && peer == SERVER {
            self.close(String::from("the connection to the server was lost"));
        }
    }

    /// The transport could not connect (a client), or the game is shutting the session down.
    pub fn connection_failed(&mut self, reason: &str) {
        self.close(String::from(reason));
    }

    fn close(&mut self, reason: String) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.connected = false;
        self.events.push_back(Event::Disconnected(reason));
    }

    /// A packet from `peer`.
    pub fn receive(&mut self, peer: PeerId, bytes: &[u8], now_us: u64) {
        match self.emul_in.as_mut() {
            Some(e) => e.push(now_us, (peer, bytes.to_vec()), None),
            None => self.process(peer, bytes, now_us),
        }
    }

    fn process(&mut self, peer: PeerId, bytes: &[u8], now_us: u64) {
        if self.closed {
            return;
        }
        self.stats.packets_in += 1;
        self.stats.bytes_in += bytes.len() as u64;
        let msg = match wire::decode(bytes) {
            Ok(m) => m,
            Err(_) => {
                self.stats.decode_errors += 1;
                return;
            }
        };
        let tick = self.clock.tick();
        if let Some(p) = self.peers.get_mut(peer) {
            p.last_heard_tick = tick;
        }
        match msg {
            Message::Hello(h) if self.role == Role::Server => self.on_hello(peer, h, now_us),
            Message::Welcome(w) if self.role == Role::Client && peer == SERVER => {
                self.on_welcome(w, now_us)
            }
            Message::Reject(r) if self.role == Role::Client && peer == SERVER => {
                self.close(r.reason)
            }
            Message::Ping(p) => {
                let pong = Pong {
                    echo_us: p.sent_us,
                    tick: self.clock.tick(),
                };
                self.send(peer, &Message::Pong(pong), now_us);
            }
            Message::Entity(a) => self.on_entity(peer, a, now_us),
            Message::Snapshot(body) if self.role == Role::Client && peer == SERVER => {
                let entities = &self.entities;
                let applied = self
                    .repl
                    .on_snapshot(&body, &|id| entities.get(id).map(|r| r.scene));
                self.route_updates();
                if let Some(t) = applied {
                    self.prediction.mark_confirmed(t);
                    if self.hash.on {
                        self.hash.rx.applied(t);
                    }
                }
                // WP5: every arrival measures the jitter, a stale (overtaken) one too.
                let tick = applied.or_else(|| crate::quant::BitReader::new(&body).read_varint());
                if let Some(t) = tick {
                    self.interp.timeline.on_snapshot(t, now_us);
                }
            }
            Message::Field(f) if self.role == Role::Client && peer == SERVER => {
                let entities = &self.entities;
                self.repl
                    .on_field(f, &|id| entities.get(id).map(|r| r.scene));
                self.route_updates();
            }
            Message::Input(body) if self.role == Role::Server && self.peers.is_joined(peer) => {
                self.on_input(peer, &body, tick);
            }
            Message::Ack(a) if self.role == Role::Server && self.peers.is_joined(peer) => {
                self.repl.on_ack(peer, a, tick);
            }
            Message::Call(c) => self.on_call(peer, c, now_us),
            Message::OwnerState(m) if self.role == Role::Server && self.peers.is_joined(peer) => {
                self.on_state(peer, m, false, now_us);
            }
            Message::Write(m) if self.role == Role::Server && self.peers.is_joined(peer) => {
                self.on_state(peer, m, true, now_us);
            }
            Message::WriteReject(r) if self.role == Role::Client && peer == SERVER => {
                self.on_write_reject(r);
            }
            Message::Keyed(m) => {
                self.keyed.stats.messages_received += 1;
                self.keyed.stats.bytes_received += bytes.len() as u64;
                self.on_keyed(peer, m, now_us);
            }
            Message::CallReturn(r) if self.role == Role::Client && peer == SERVER => {
                self.on_call_return(r);
            }
            Message::Hash(m) if self.role == Role::Client && peer == SERVER => {
                if self.hash.on {
                    self.hash.rx.on_msg(m);
                }
            }
            Message::Pong(p) => {
                let rtt = now_us.saturating_sub(p.echo_us) as f64;
                if let Some(state) = self.peers.get_mut(peer) {
                    state.rtt.sample(rtt);
                }
                if self.role == Role::Client && peer == SERVER {
                    self.clock.observe_server_tick(p.tick, rtt);
                }
            }
            _ => {}
        }
    }

    fn on_hello(&mut self, peer: PeerId, hello: Hello, now_us: u64) {
        self.peers.insert(peer, self.clock.tick());
        if self
            .peers
            .get(peer)
            .is_some_and(|p| p.stage != Stage::Connecting)
        {
            return;
        }
        if let Some(reason) =
            self.protocol
                .mismatch(&hello.items, hello.protocol_hash, "server", "client")
        {
            let tick = self.clock.tick();
            if let Some(p) = self.peers.get_mut(peer) {
                p.stage = Stage::Rejected(tick);
            }
            self.send(peer, &Message::Reject(Reject { reason }), now_us);
            return;
        }
        let id_range = IdRange {
            start: self.next_slot.saturating_mul(ID_RANGE_LEN),
            len: ID_RANGE_LEN,
        };
        self.next_slot += 1;
        if let Some(p) = self.peers.get_mut(peer) {
            p.stage = Stage::Joined;
            p.id_range = id_range;
        }
        let welcome = Welcome {
            protocol_hash: self.protocol.hash(),
            tick_rate: self.tick_rate(),
            peer_id: peer,
            id_range,
            current_tick: self.clock.tick(),
        };
        self.send(peer, &Message::Welcome(welcome), now_us);
        self.ping(peer, now_us);
        // The late-join burst: the whole entity table as creates, parents first.
        self.scenes_sent.remove(&peer);
        let tick = self.clock.tick();
        self.repl.add_peer(peer, tick);
        // WP3: each create carries the entity's latest full state, and the peer is then known to hold
        // it (netcore keeps the latest values, so the burst reproduces current state).
        for mut c in self.entities.burst() {
            let id = c.id;
            if self.repl.entity(id).is_some() {
                c.initial_state = self.repl.encode_full(id);
            }
            self.send_entity(peer, EntityAction::Create(c), now_us);
            self.repl.mark_synced(peer, id);
        }
        // WP7: the keyed state it can see (viewed paths follow once gdnet decides) and the rules.
        let full = KeyMsg::Full {
            entries: self.keyed.entries_for(peer),
            rules: self.keyed.rules().to_vec(),
        };
        self.send_keyed(peer, full, now_us);
        self.events.push_back(Event::PeerJoined(peer));
    }

    fn on_welcome(&mut self, w: Welcome, now_us: u64) {
        if self.connected
            || !self
                .peers
                .get(SERVER)
                .is_some_and(|p| p.stage == Stage::Connecting)
        {
            return;
        }
        if w.protocol_hash != self.protocol.hash() || w.tick_rate != self.tick_rate() {
            self.close(String::from(
                "protocol mismatch: the server's welcome does not match this client's protocol",
            ));
            return;
        }
        let rtt = self.hello_sent_us.map(|t| now_us.saturating_sub(t) as f64);
        let half_rtt_ticks = rtt.map_or(0.0, |r| (r / 2.0) / self.clock.tick_us());
        self.clock
            .set_tick(w.current_tick + half_rtt_ticks.round() as u64);
        if let Some(r) = rtt {
            self.clock.observe_server_tick(w.current_tick, r);
        }
        self.local_id = w.peer_id;
        self.id_range = w.id_range;
        self.ids = IdAllocator::new(w.id_range);
        if let Some(p) = self.peers.get_mut(SERVER) {
            p.stage = Stage::Joined;
            if let Some(r) = rtt {
                p.rtt.sample(r);
            }
        }
        self.connected = true;
        self.events.push_back(Event::Connected {
            local_id: self.local_id,
            tick: self.clock.tick(),
        });
        self.events.push_back(Event::PeerJoined(SERVER));
        self.ping(SERVER, now_us);
    }

    /// One fixed tick (a physics tick): advances the clock, sends due pings, releases emulated packets,
    /// and asks to drop rejected peers after a second.
    pub fn tick(&mut self, now_us: u64) {
        if let Some(e) = self.emul_in.as_mut() {
            for (peer, bytes) in e.pop_ready(now_us) {
                self.process(peer, &bytes, now_us);
            }
        }
        if self.closed {
            return;
        }
        let tick = self.clock.advance();
        for h in self.held.overdue(tick, self.held_timeout) {
            let what = held_description(&h.action, &self.scene_paths);
            self.events.push_back(Event::EntityError(std::format!(
                "gdnet: {what} from peer {} has waited {} ticks for an entity that has not arrived; it is kept and applied when that entity arrives",
                h.from,
                tick.saturating_sub(h.since)
            )));
        }
        if self.role == Role::Server && !self.started {
            self.started = true;
            self.connected = true;
            self.events.push_back(Event::Connected {
                local_id: SERVER,
                tick,
            });
        }
        if tick.is_multiple_of(self.ping_interval) {
            for peer in self.peers.joined() {
                self.ping(peer, now_us);
            }
        }
        let rate = u64::from(self.tick_rate());
        let expired: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|p| matches!(p.stage, Stage::Rejected(t) if tick >= t + rate))
            .map(|p| p.id)
            .collect();
        for peer in expired {
            self.peers.remove(peer);
            self.events.push_back(Event::DisconnectPeer {
                peer,
                reason: String::from("handshake rejected"),
            });
        }
        self.input_tick(tick);
        self.expire_queries(tick, now_us);
        self.expire_calls(now_us);
        self.flush_emulated(now_us);
    }

    fn flush_emulated(&mut self, now_us: u64) {
        if let Some(e) = self.emul_out.as_mut() {
            let ready = e.pop_ready(now_us);
            for p in ready {
                self.queue_out(p);
            }
        }
    }

    /// Packets to hand to the transport, in order.
    pub fn outgoing(&mut self) -> Vec<Outgoing> {
        core::mem::take(&mut self.out)
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

/// What [`Session::spawn`] creates: where, under which name, for whom, with how many parts and which
/// initial state.
#[derive(Clone, Debug, PartialEq)]
pub struct Spawn {
    pub parent: ParentRef,
    pub name: String,
    pub owner: PeerId,
    pub parts: u16,
    pub initial_state: Vec<u8>,
    /// WP3: every part's property values (in table order). When given and the scene has a property
    /// table, netcore records them and encodes `initial_state` itself.
    pub values: Option<Vec<Vec<Value>>>,
    /// WP7: the transfer rule and whether the owner's state is accepted (a client's own spawn is
    /// always `client_owned`).
    pub transfer: Transfer,
    pub client_owned: bool,
    /// WP7: take the parent entity's owner instead of `owner` (nested entities, 2.6).
    pub inherit_owner: bool,
}

impl Spawn {
    pub fn new(
        parent: ParentRef,
        name: &str,
        owner: PeerId,
        parts: u16,
        initial_state: Vec<u8>,
    ) -> Self {
        Self {
            parent,
            name: String::from(name),
            owner,
            parts,
            initial_state,
            values: None,
            transfer: Transfer::Fixed,
            client_owned: false,
            inherit_owner: false,
        }
    }

    /// WP7: the entity's transfer rule and `client_owned` flag.
    pub fn with_authority(mut self, transfer: Transfer, client_owned: bool) -> Self {
        self.transfer = transfer;
        self.client_owned = client_owned;
        self
    }

    /// WP7: the entity takes its parent entity's owner (when it has a parent entity).
    pub fn inheriting_owner(mut self) -> Self {
        self.inherit_owner = true;
        self
    }

    /// WP3: the entity's property values per part (they become the `Create`'s initial state).
    pub fn with_values(mut self, values: Vec<Vec<Value>>) -> Self {
        self.values = Some(values);
        self
    }
}

/// A readable name for a held action, for the "still waiting" error.
fn held_description(a: &EntityAction, paths: &BTreeMap<u64, String>) -> String {
    let scene = |h: u64| {
        paths
            .get(&h)
            .cloned()
            .unwrap_or_else(|| std::format!("{h:016x}"))
    };
    match a {
        EntityAction::Create(c) => std::format!(
            "the create of entity {} ({}, scene {}) under entity {}",
            c.id,
            c.name,
            scene(c.scene_hash),
            c.parent.entity
        ),
        EntityAction::Destroy { id } => std::format!("the destroy of entity {id}"),
        EntityAction::Reparent { id, parent } => {
            std::format!("the reparent of entity {id} under entity {}", parent.entity)
        }
        EntityAction::SetOwner { id, .. } => std::format!("the owner change of entity {id}"),
        EntityAction::RequestOwner { id } => std::format!("an ownership request for entity {id}"),
        EntityAction::ReleaseOwner { id } => std::format!("the release of entity {id}"),
        EntityAction::OwnerQuery { id, .. }
        | EntityAction::OwnerAnswer { id, .. }
        | EntityAction::OwnerReply { id, .. } => {
            std::format!("an ownership decision on entity {id}")
        }
        EntityAction::Scene { path, .. } => std::format!("the scene {path}"),
        EntityAction::Call(c) => std::format!("a call on entity {}", c.entity),
    }
}

/// The entity layer (WP2): the table, id allocation, `EntityAction` traffic and the scene part checks.
impl Session {
    /// Every entity this side knows.
    pub fn entities(&self) -> &EntityTable {
        &self.entities
    }

    /// Entity actions waiting for an entity they name.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// The path a scene hash stands for, when this side registered it or a peer announced it.
    pub fn scene_path(&self, hash: u64) -> Option<&str> {
        self.scene_paths.get(&hash).map(String::as_str)
    }

    /// The next entity event for gdnet to apply.
    pub fn poll_entity_event(&mut self) -> Option<EntityEvent> {
        self.entity_events.pop_front()
    }

    /// Registers this side's part table hash for a scene (a resource path, or a class name for a bare
    /// node) and returns the scene hash. Compares it with what peers announced for the same scene: a
    /// client whose table differs from the server's ends its session; on the server a client whose
    /// table differs is refused. Registering again with the same hash is cheap.
    pub fn register_scene(&mut self, path: &str, parts_hash: u64) -> u64 {
        let hash = crate::entity::scene_hash(path);
        if self.protocol.scene(path) != Some(parts_hash) {
            self.protocol.register_scene(path, parts_hash);
        }
        self.scene_paths
            .entry(hash)
            .or_insert_with(|| String::from(path));
        let announced: Vec<(PeerId, u64)> = self
            .remote_scenes
            .iter()
            .filter(|((_, h), _)| *h == hash)
            .map(|((p, _), v)| (*p, *v))
            .collect();
        for (peer, theirs) in announced {
            self.check_scene(peer, path, theirs, 0);
        }
        hash
    }

    /// Compares a peer's part table hash for `path` with this side's; acts on a mismatch. True when
    /// they agree (or this side has not registered the scene).
    fn check_scene(&mut self, peer: PeerId, path: &str, theirs: u64, now_us: u64) -> bool {
        let (local, remote) = match self.role {
            Role::Server => ("server", "client"),
            Role::Client => ("client", "server"),
        };
        let Some(reason) = self.protocol.scene_mismatch(path, theirs, local, remote) else {
            return true;
        };
        match self.role {
            Role::Client => self.close(reason),
            Role::Server => self.reject_peer(peer, reason, now_us),
        }
        false
    }

    /// Server: refuses a peer after the handshake (a scene mismatch, a flood of held actions).
    fn reject_peer(&mut self, peer: PeerId, reason: String, now_us: u64) {
        let tick = self.clock.tick();
        let was_joined = self.peers.is_joined(peer);
        if let Some(p) = self.peers.get_mut(peer) {
            if matches!(p.stage, Stage::Rejected(_)) {
                return;
            }
            p.stage = Stage::Rejected(tick);
        }
        self.send(peer, &Message::Reject(Reject { reason }), now_us);
        if was_joined {
            self.events.push_back(Event::PeerLeft(peer));
        }
    }

    /// Sends an entity action to one peer; a `Create` is preceded by the scene's announcement the first
    /// time that peer needs it.
    fn send_entity(&mut self, peer: PeerId, action: EntityAction, now_us: u64) {
        if let EntityAction::Create(c) = &action {
            let sent = self.scenes_sent.entry(peer).or_default();
            if !sent.contains(&c.scene_hash) {
                if let Some(path) = self.scene_paths.get(&c.scene_hash).cloned() {
                    sent.insert(c.scene_hash);
                    let parts_hash = self.protocol.scene(&path).unwrap_or(0);
                    let props = self.repl.table(c.scene_hash).cloned().unwrap_or_default();
                    self.send(
                        peer,
                        &Message::Entity(EntityAction::Scene {
                            hash: c.scene_hash,
                            path,
                            parts_hash,
                            props,
                        }),
                        now_us,
                    );
                }
            }
        }
        self.send(peer, &Message::Entity(action), now_us);
    }

    /// The server sends to every joined peer but `except`; a client sends to the server.
    fn share(&mut self, action: EntityAction, except: Option<PeerId>, now_us: u64) {
        let targets: Vec<PeerId> = match self.role {
            Role::Server => self
                .peers
                .joined()
                .into_iter()
                .filter(|p| Some(*p) != except)
                .collect(),
            Role::Client => std::vec![SERVER],
        };
        for peer in targets {
            self.send_entity(peer, action.clone(), now_us);
        }
    }

    /// Creates an entity of a registered scene on this side and announces it: the server to every
    /// joined peer, a client (spawn policy `owner_allowed`) to the server, which may refuse it. Returns
    /// the new id.
    pub fn spawn(&mut self, scene_path: &str, spec: Spawn, now_us: u64) -> Result<NetId, String> {
        let Spawn {
            parent,
            name,
            mut owner,
            parts,
            mut initial_state,
            values,
            transfer,
            mut client_owned,
            inherit_owner,
        } = spec;
        if self.role == Role::Client {
            client_owned = true;
        } else if inherit_owner {
            if let Some(p) = self.entities.get(parent.entity) {
                owner = p.owner;
            }
        }
        if !self.is_connected() {
            return Err(String::from("the session is not connected"));
        }
        let scene_hash = crate::entity::scene_hash(scene_path);
        if self.protocol.scene(scene_path).is_none() {
            return Err(std::format!("the scene {scene_path} is not registered"));
        }
        if parent.entity != crate::entity::MANAGER_ROOT && !self.entities.contains(parent.entity) {
            return Err(std::format!(
                "the parent entity {} is unknown",
                parent.entity
            ));
        }
        let Some(id) = self.ids.allocate() else {
            return Err(String::from("this peer's entity id range is used up"));
        };
        let tick = self.clock.tick();
        if let Some(values) = values {
            if self.repl.table(scene_hash).is_some() {
                initial_state = self.repl.add_entity(id, scene_hash, &values, tick);
            }
        }
        let create = CreateAction {
            id,
            scene_hash,
            parent,
            name,
            owner,
            parts,
            initial_state,
            transfer,
            client_owned,
        };
        self.entities.insert(&create);
        self.share(EntityAction::Create(create), None, now_us);
        if self.repl.entity(id).is_some() {
            for peer in self.peers.joined() {
                self.repl.mark_synced(peer, id);
            }
        }
        Ok(id)
    }

    /// Destroys an entity this side may destroy (the server: any; a client: one it created) and
    /// announces it. Returns the removed records, children first (empty when unknown or not allowed).
    pub fn destroy(&mut self, id: NetId, now_us: u64) -> Vec<EntityRecord> {
        if !self.may_change(id) {
            return Vec::new();
        }
        let removed = self.entities.remove(id);
        for r in &removed {
            self.repl.remove_entity(r.id);
            self.repl.forget_received(r.id);
            self.forget_input(r.id);
        }
        if !removed.is_empty() {
            self.share(EntityAction::Destroy { id }, None, now_us);
        }
        removed
    }

    /// Moves an entity this side may change under `parent` and announces it.
    pub fn reparent(&mut self, id: NetId, parent: ParentRef, now_us: u64) -> bool {
        if !self.may_change(id) || !self.entities.reparent(id, parent) {
            return false;
        }
        self.share(EntityAction::Reparent { id, parent }, None, now_us);
        true
    }

    /// Server only: changes an entity's owner and announces it.
    pub fn set_owner(&mut self, id: NetId, owner: PeerId, now_us: u64) -> bool {
        if self.role != Role::Server {
            return false;
        }
        let Some((old, manager)) = self.entities.get(id).map(|r| (r.owner, r.parent.manager))
        else {
            return false;
        };
        if old == owner {
            return true;
        }
        self.entities.set_owner(id, owner);
        // A new owner starts a new input stream and a new owner-state stream.
        self.input_buffers.remove(&id);
        self.server_inputs.remove(&id);
        self.auth.owner_tick.remove(&id);
        self.auth.stats.owner_changes += 1;
        let stale: Vec<(NetId, PeerId)> = self
            .auth
            .queries
            .keys()
            .filter(|(q, _)| *q == id)
            .copied()
            .collect();
        for (q, r) in stale {
            self.auth.queries.remove(&(q, r));
            if r != owner {
                self.reply_owner(q, r, false, now_us);
            }
        }
        // WP7: the server's gdnet hears of every change too (owner_changed fires on every peer).
        self.entity_events
            .push_back(EntityEvent::SetOwner { id, manager, owner });
        self.share(EntityAction::SetOwner { id, owner }, None, now_us);
        true
    }

    fn may_change(&self, id: NetId) -> bool {
        match self.role {
            Role::Server => true,
            Role::Client => {
                self.is_connected()
                    && IdAllocator::in_range(self.id_range, id)
                    && self
                        .entities
                        .get(id)
                        .is_some_and(|r| r.owner == self.local_id)
            }
        }
    }

    /// Server: admits a client's proposed create (it joins the table and goes to every other peer).
    /// False when the proposal is unknown or its parent has gone meanwhile (then it is refused).
    pub fn accept_create(&mut self, id: NetId, now_us: u64) -> bool {
        let Some((create, from)) = self.proposals.remove(&id) else {
            return false;
        };
        if !self.entities.insert(&create) {
            self.send(from, &Message::Entity(EntityAction::Destroy { id }), now_us);
            self.drain_held(now_us);
            return false;
        }
        // WP7: the creator's initial state is the server's record of it (relayed, and in bursts).
        self.record_encoded(id, create.scene_hash, &create.initial_state);
        self.share(EntityAction::Create(create), Some(from), now_us);
        if self.repl.entity(id).is_some() {
            for peer in self.peers.joined() {
                self.repl.mark_synced(peer, id);
            }
        }
        self.drain_held(now_us);
        true
    }

    /// Server: refuses a client's proposed create; the client is told to destroy it.
    pub fn refuse_create(&mut self, id: NetId, now_us: u64) {
        if let Some((_, from)) = self.proposals.remove(&id) {
            self.send(from, &Message::Entity(EntityAction::Destroy { id }), now_us);
        }
        self.drain_held(now_us);
    }

    fn expected(&self) -> BTreeSet<NetId> {
        let mut e = self.held.expected();
        e.extend(self.proposals.keys().copied());
        e
    }

    fn is_ready(&self, a: &EntityAction, expected: &BTreeSet<NetId>) -> bool {
        let (target, parent) = a.needs();
        target.is_none_or(|t| self.entities.contains(t) || !expected.contains(&t))
            && parent.is_none_or(|p| self.entities.contains(p))
    }

    fn on_entity(&mut self, peer: PeerId, action: EntityAction, now_us: u64) {
        if let EntityAction::Scene {
            hash,
            path,
            parts_hash,
            props,
        } = &action
        {
            if self.role == Role::Client && peer != SERVER {
                return;
            }
            self.scene_paths
                .entry(*hash)
                .or_insert_with(|| path.clone());
            self.remote_scenes.insert((peer, *hash), *parts_hash);
            let path = path.clone();
            let (hash, props) = (*hash, props.clone());
            if self.check_scene(peer, &path, *parts_hash, now_us) && !props.parts.is_empty() {
                self.remote_props.insert((peer, hash), props);
                self.check_props(peer, hash, now_us);
            }
            return;
        }
        if !self.admits(peer, &action) {
            self.stats.entity_refused += 1;
            return;
        }
        let expected = self.expected();
        let (_, parent) = action.needs();
        let behind_held = !self.held.is_empty()
            && [action.subject(), parent]
                .into_iter()
                .flatten()
                .any(|id| self.held.names(id));
        if behind_held || !self.is_ready(&action, &expected) {
            if self.held.len() >= MAX_HELD {
                let reason = std::format!(
                    "gdnet: more than {MAX_HELD} entity actions are waiting for entities that never arrived"
                );
                match self.role {
                    Role::Client => self.close(reason),
                    Role::Server => self.reject_peer(peer, reason, now_us),
                }
                return;
            }
            self.held.push(peer, action, self.clock.tick());
            return;
        }
        self.apply(peer, action, now_us);
        self.drain_held(now_us);
    }

    /// Who may send what: a client takes actions from the server only; the server takes creates of
    /// client-owned entities in the sender's id range, and destroys and reparents of entities the
    /// sender created and owns.
    fn admits(&self, peer: PeerId, action: &EntityAction) -> bool {
        match self.role {
            Role::Client => peer == SERVER && self.is_connected(),
            Role::Server => {
                let Some(state) = self.peers.get(peer) else {
                    return false;
                };
                if state.stage != Stage::Joined {
                    return false;
                }
                let range = state.id_range;
                match action {
                    EntityAction::Create(c) => {
                        c.owner == peer
                            && IdAllocator::in_range(range, c.id)
                            && !self.entities.contains(c.id)
                            && !self.proposals.contains_key(&c.id)
                    }
                    EntityAction::Destroy { id } | EntityAction::Reparent { id, .. } => {
                        IdAllocator::in_range(range, *id)
                            && self.entities.get(*id).is_none_or(|r| r.owner == peer)
                    }
                    EntityAction::SetOwner { .. }
                    | EntityAction::Scene { .. }
                    | EntityAction::OwnerQuery { .. }
                    | EntityAction::OwnerReply { .. } => false,
                    // WP7: checked when applied (the entity and its owner may change meanwhile).
                    EntityAction::RequestOwner { .. }
                    | EntityAction::ReleaseOwner { .. }
                    | EntityAction::OwnerAnswer { .. } => true,
                    // WP6: admitted by `on_call` (sender set from the transport).
                    EntityAction::Call(_) => true,
                }
            }
        }
    }

    fn drain_held(&mut self, now_us: u64) {
        loop {
            let expected = self.expected();
            let entities = &self.entities;
            let ready = |a: &EntityAction| {
                let (target, parent) = a.needs();
                target.is_none_or(|t| entities.contains(t) || !expected.contains(&t))
                    && parent.is_none_or(|p| entities.contains(p))
            };
            let Some(h) = self.held.take_ready(ready) else {
                return;
            };
            self.apply(h.from, h.action, now_us);
        }
    }

    fn apply(&mut self, from: PeerId, action: EntityAction, now_us: u64) {
        match action {
            EntityAction::Scene { .. } => {}
            EntityAction::Call(c) => self.deliver_call(from, c, now_us),
            EntityAction::Create(create) => {
                let scene_path = self
                    .scene_paths
                    .get(&create.scene_hash)
                    .cloned()
                    .unwrap_or_default();
                if self.role == Role::Server && from != SERVER {
                    self.proposals.insert(create.id, (create.clone(), from));
                    self.entity_events.push_back(EntityEvent::Create {
                        create,
                        scene_path,
                        from,
                        proposal: true,
                    });
                } else if self.entities.insert(&create) {
                    self.entity_events.push_back(EntityEvent::Create {
                        create,
                        scene_path,
                        from,
                        proposal: false,
                    });
                }
            }
            EntityAction::Destroy { id } => {
                let removed = self.entities.remove(id);
                if removed.is_empty() {
                    return;
                }
                for r in &removed {
                    self.repl.remove_entity(r.id);
                    self.repl.forget_received(r.id);
                    self.forget_input(r.id);
                }
                let n = removed.len();
                for (i, record) in removed.into_iter().enumerate() {
                    self.entity_events.push_back(EntityEvent::Destroy {
                        record,
                        cascade: i + 1 < n,
                    });
                }
                if self.role == Role::Server {
                    self.share(EntityAction::Destroy { id }, Some(from), now_us);
                }
            }
            EntityAction::Reparent { id, parent } => {
                let from_manager = self.entities.get(id).map(|r| r.parent.manager);
                if let Some(from_manager) = from_manager {
                    if self.entities.reparent(id, parent) {
                        self.entity_events.push_back(EntityEvent::Reparent {
                            id,
                            from_manager,
                            parent,
                        });
                        if self.role == Role::Server {
                            self.share(EntityAction::Reparent { id, parent }, Some(from), now_us);
                        }
                    }
                }
            }
            EntityAction::SetOwner { id, owner } => {
                let manager = self.entities.get(id).map(|r| r.parent.manager);
                if let Some(manager) = manager {
                    if self.entities.set_owner(id, owner) {
                        self.auth.stats.owner_changes += 1;
                        self.auth.last_sent.remove(&id);
                        self.auth.pending_writes.retain(|k| k.0 != id);
                        self.entity_events
                            .push_back(EntityEvent::SetOwner { id, manager, owner });
                    }
                }
            }
            EntityAction::RequestOwner { id } => {
                if self.role == Role::Server {
                    self.handle_request(id, from, now_us);
                }
            }
            EntityAction::ReleaseOwner { id } => {
                if self.role == Role::Server
                    && self.entities.get(id).is_some_and(|r| r.owner == from)
                {
                    self.set_owner(id, SERVER, now_us);
                }
            }
            EntityAction::OwnerAnswer {
                id,
                requester,
                grant,
            } => {
                let asked = self.auth.queries.contains_key(&(id, requester))
                    && self.entities.get(id).is_some_and(|r| r.owner == from);
                if self.role == Role::Server && asked {
                    self.finish_query(id, requester, grant, now_us);
                }
            }
            EntityAction::OwnerQuery { id, requester } => {
                if let Some(manager) = self.entities.get(id).map(|r| r.parent.manager) {
                    self.entity_events.push_back(EntityEvent::OwnerQuery {
                        id,
                        manager,
                        requester,
                    });
                }
            }
            EntityAction::OwnerReply { id, granted } => {
                if granted {
                    self.auth.stats.requests_granted += 1;
                } else {
                    self.auth.stats.requests_refused += 1;
                }
                if let Some(manager) = self.entities.get(id).map(|r| r.parent.manager) {
                    self.entity_events.push_back(EntityEvent::OwnerReply {
                        id,
                        manager,
                        granted,
                    });
                }
            }
        }
    }
}

/// Calls (WP6, call.rs): routing, trust hand-off to gdnet, request ids and timeouts.
impl Session {
    /// Registers this side's method tables for a scene (per part, the hashes of its exposed method
    /// names). A received call naming a hash its part does not have is answered with an error (and
    /// counted), not delivered. Scenes without a table are delivered unchecked.
    pub fn register_methods(&mut self, scene: u64, mut parts: Vec<Vec<u32>>) {
        for p in &mut parts {
            p.sort_unstable();
            p.dedup();
        }
        self.calls.methods.insert(scene, parts);
    }

    /// Changes how long a request waits for its answer (default 10 s).
    pub fn set_call_timeout_us(&mut self, us: u64) {
        self.calls.timeout_us = us.max(1);
    }

    pub fn call_stats(&self) -> CallStats {
        self.calls.stats
    }

    /// Requests waiting for a `CallReturn`.
    pub fn pending_requests(&self) -> usize {
        self.calls.pending.len()
    }

    /// The next resolved request (its `CallReturn`, or a timeout).
    pub fn poll_call_result(&mut self) -> Option<CallResult> {
        self.calls.results.pop_front()
    }

    /// Makes a call from this side: decides who runs it (the module table of call.rs), sends what must
    /// be sent, and says whether the caller runs it now. Errors (not connected, an unknown entity, a
    /// target peer that is not connected) send nothing.
    pub fn call(&mut self, c: OutCall, now_us: u64) -> Result<CallPlan, String> {
        if !self.is_connected() {
            return Err(String::from("the session is not connected"));
        }
        let Some(owner) = self.entities.get(c.entity).map(|r| r.owner) else {
            return Err(std::format!(
                "entity {} is not spawned on this peer",
                c.entity
            ));
        };
        let local = self.local_id;
        let mut msg = CallMsg {
            entity: c.entity,
            part: c.part,
            method: c.method,
            verb: c.verb,
            target: if c.verb == Verb::Peer { c.target } else { 0 },
            sender: local,
            request: 0,
            tick: self.clock.tick(),
            unreliable: c.unreliable,
            args: c.args,
        };
        let mut plan = CallPlan::default();
        match self.role {
            Role::Server => match c.verb {
                Verb::Sync => {
                    plan.run_local = true;
                    plan.sent = self.broadcast_call(&msg, None, now_us);
                }
                Verb::Server => plan.run_local = true,
                Verb::Others => plan.sent = self.broadcast_call(&msg, None, now_us),
                Verb::Owner | Verb::Peer => {
                    let to = if c.verb == Verb::Owner {
                        owner
                    } else {
                        c.target
                    };
                    if to == SERVER {
                        plan.run_local = true;
                    } else if self.peers.is_joined(to) {
                        self.send_call(to, msg, now_us);
                        plan.sent = true;
                    } else {
                        return Err(std::format!("peer {to} is not connected"));
                    }
                }
            },
            Role::Client => {
                let mine = match c.verb {
                    Verb::Owner => owner == local,
                    Verb::Peer => c.target == local,
                    _ => false,
                };
                if mine {
                    plan.run_local = true;
                    return Ok(plan);
                }
                if !msg.unreliable {
                    self.calls.next_request = self.calls.next_request.wrapping_add(1).max(1);
                    let r = self.calls.next_request;
                    self.calls
                        .pending
                        .insert(r, now_us.saturating_add(self.calls.timeout_us));
                    msg.request = r;
                    plan.request = Some(r);
                }
                self.send_call(SERVER, msg, now_us);
                plan.sent = true;
            }
        }
        Ok(plan)
    }

    /// Server: gdnet ran (or checked) the call `handle` it was given; `result` is the encoded return
    /// value, or an error for the caller. On success the call goes on as its verb says (a `Sync` to
    /// every peer in scope, the caller included; `Others` to everyone but the caller; `Owner` / `Peer`
    /// to their target) and the caller's request is answered.
    pub fn finish_call(&mut self, handle: u64, result: Result<Vec<u8>, String>, now_us: u64) {
        let Some((c, from)) = self.calls.inbound.remove(&handle) else {
            return;
        };
        let value = match result {
            Err(e) => {
                if c.request != 0 {
                    self.send_return(from, c.request, Err(e), now_us);
                }
                return;
            }
            Ok(v) => v,
        };
        let relay = CallMsg {
            request: 0,
            sender: from,
            ..c.clone()
        };
        match c.verb {
            Verb::Sync => {
                self.broadcast_call(&relay, None, now_us);
            }
            Verb::Others => {
                self.broadcast_call(&relay, Some(from), now_us);
            }
            Verb::Server => {}
            Verb::Owner | Verb::Peer => {
                let to = if c.verb == Verb::Owner {
                    self.entities.get(c.entity).map_or(SERVER, |r| r.owner)
                } else {
                    c.target
                };
                if to != SERVER && self.peers.is_joined(to) {
                    self.send_call(to, relay, now_us);
                }
            }
        }
        if c.request != 0 {
            let v = if c.verb == Verb::Server {
                value
            } else {
                Vec::new()
            };
            self.send_return(from, c.request, Ok(v), now_us);
        }
    }

    /// Server: gdnet refused the call `handle` (the trust check of D14); the caller gets `reason`.
    pub fn refuse_call(&mut self, handle: u64, reason: &str, now_us: u64) {
        if self.calls.inbound.contains_key(&handle) {
            self.calls.stats.refused += 1;
            self.finish_call(handle, Err(String::from(reason)), now_us);
        }
    }

    /// Interest hook: whether `peer` has entity `id` in scope (every joined peer until WP7/WP13 add
    /// interest management).
    fn in_scope(&self, _peer: PeerId, _id: NetId) -> bool {
        true
    }

    fn send_call(&mut self, peer: PeerId, msg: CallMsg, now_us: u64) {
        self.calls.stats.sent += 1;
        self.send(peer, &Message::Call(msg), now_us);
    }

    /// The server sends `msg` to every joined peer in scope but `except`. True when any was sent.
    fn broadcast_call(&mut self, msg: &CallMsg, except: Option<PeerId>, now_us: u64) -> bool {
        let targets: Vec<PeerId> = self
            .peers
            .joined()
            .into_iter()
            .filter(|p| Some(*p) != except && self.in_scope(*p, msg.entity))
            .collect();
        for &p in &targets {
            self.send_call(p, msg.clone(), now_us);
        }
        !targets.is_empty()
    }

    fn send_return(
        &mut self,
        peer: PeerId,
        request: u32,
        result: Result<Vec<u8>, String>,
        now_us: u64,
    ) {
        let ret = match result {
            Ok(value) => CallReturn {
                request,
                ok: true,
                value,
                error: String::new(),
            },
            Err(error) => CallReturn {
                request,
                ok: false,
                value: Vec::new(),
                error,
            },
        };
        self.send(peer, &Message::CallReturn(ret), now_us);
    }

    /// A received call: a client takes calls from the server only; the server takes them from joined
    /// peers and stamps the sender from the transport. It then goes through the held queue, so it waits
    /// behind its entity's pending `Create`.
    fn on_call(&mut self, peer: PeerId, mut c: CallMsg, now_us: u64) {
        match self.role {
            Role::Client => {
                if peer != SERVER || !self.is_connected() {
                    return;
                }
            }
            Role::Server => {
                if !self.peers.is_joined(peer) {
                    return;
                }
                c.sender = peer;
            }
        }
        self.calls.stats.received += 1;
        self.on_entity(peer, EntityAction::Call(c), now_us);
    }

    fn on_call_return(&mut self, r: CallReturn) {
        self.calls.stats.returns += 1;
        if self.calls.pending.remove(&r.request).is_some() {
            let result = if r.ok { Ok(r.value) } else { Err(r.error) };
            self.calls.results.push_back(CallResult {
                request: r.request,
                result,
            });
        }
    }

    fn expire_calls(&mut self, now_us: u64) {
        let expired: Vec<u32> = self
            .calls
            .pending
            .iter()
            .filter(|(_, d)| now_us >= **d)
            .map(|(r, _)| *r)
            .collect();
        let secs = self.calls.timeout_us as f64 / 1_000_000.0;
        for r in expired {
            self.calls.pending.remove(&r);
            self.calls.stats.timeouts += 1;
            self.calls.results.push_back(CallResult {
                request: r,
                result: Err(std::format!(
                    "the call timed out after {secs} s without an answer from the server"
                )),
            });
        }
    }

    fn method_known(&self, scene: u64, part: u16, method: u32) -> bool {
        match self.calls.methods.get(&scene) {
            None => true,
            Some(parts) => parts
                .get(usize::from(part))
                .is_some_and(|p| p.binary_search(&method).is_ok()),
        }
    }

    /// A call whose entity is ready (or gone): checks it and hands it to gdnet.
    fn deliver_call(&mut self, from: PeerId, c: CallMsg, now_us: u64) {
        let Some((manager, owner, scene)) = self
            .entities
            .get(c.entity)
            .map(|r| (r.parent.manager, r.owner, r.scene))
        else {
            self.calls.stats.dropped += 1;
            if self.role == Role::Server && c.request != 0 {
                let e = std::format!("entity {} was destroyed before the call arrived", c.entity);
                self.send_return(from, c.request, Err(e), now_us);
            }
            return;
        };
        if !self.method_known(scene, c.part, c.method) {
            self.calls.stats.unknown_method += 1;
            if self.role == Role::Server && c.request != 0 {
                let path = self
                    .scene_paths
                    .get(&scene)
                    .cloned()
                    .unwrap_or_else(|| std::format!("{scene:016x}"));
                let e = std::format!(
                    "unknown method (hash {:08x}) on part {} of entity {} ({path}); methods beginning with _ are never callable",
                    c.method, c.part, c.entity
                );
                self.send_return(from, c.request, Err(e), now_us);
            }
            return;
        }
        let mut inc = CallIn {
            handle: 0,
            manager,
            entity: c.entity,
            part: c.part,
            method: c.method,
            verb: c.verb,
            sender: c.sender,
            tick: c.tick,
            args: c.args.clone(),
            check: false,
            run_local: true,
        };
        if self.role == Role::Server {
            if c.verb == Verb::Peer && c.target != SERVER && !self.peers.is_joined(c.target) {
                if c.request != 0 {
                    let e = std::format!("peer {} is not connected", c.target);
                    self.send_return(from, c.request, Err(e), now_us);
                }
                return;
            }
            inc.run_local = match c.verb {
                Verb::Sync | Verb::Server | Verb::Others => true,
                Verb::Owner => owner == SERVER,
                Verb::Peer => c.target == SERVER,
            };
            inc.check = c.verb != Verb::Server && owner != from;
            self.calls.next_handle += 1;
            inc.handle = self.calls.next_handle;
            self.calls.inbound.insert(inc.handle, (c, from));
        }
        self.entity_events.push_back(EntityEvent::Call(inc));
    }
}

/// State replication (WP3): property tables, recording, snapshots and acks.
impl Session {
    /// Registers this side's property table for a scene (registered with `register_scene` first) and
    /// compares it with what peers announced; a difference ends the session (client) or refuses the
    /// peer (server) with a reason naming the scene, part and property. `part_names` are the scene's
    /// part paths, for that reason. Returns false when this side's session ended because of it.
    pub fn register_props(
        &mut self,
        path: &str,
        table: ScenePropsTable,
        part_names: Vec<String>,
    ) -> bool {
        let hash = crate::entity::scene_hash(path);
        self.scene_paths
            .entry(hash)
            .or_insert_with(|| String::from(path));
        self.part_names.insert(hash, part_names);
        if self.repl.table(hash) != Some(&table) {
            self.repl.register_table(hash, table);
        }
        let peers: Vec<PeerId> = self
            .remote_props
            .keys()
            .filter(|(_, h)| *h == hash)
            .map(|(p, _)| *p)
            .collect();
        for peer in peers {
            self.check_props(peer, hash, 0);
        }
        !self.closed
    }

    /// This side's property table of a scene hash.
    pub fn props(&self, scene: u64) -> Option<&ScenePropsTable> {
        self.repl.table(scene)
    }

    fn check_props(&mut self, peer: PeerId, scene: u64, now_us: u64) -> bool {
        let (Some(ours), Some(theirs)) = (
            self.repl.table(scene),
            self.remote_props.get(&(peer, scene)),
        ) else {
            return true;
        };
        let (local, remote) = match self.role {
            Role::Server => ("server", "client"),
            Role::Client => ("client", "server"),
        };
        let path = self
            .scene_paths
            .get(&scene)
            .cloned()
            .unwrap_or_else(|| std::format!("{scene:016x}"));
        let names = self.part_names.get(&scene).cloned().unwrap_or_default();
        let name = |i: usize| {
            names
                .get(i)
                .cloned()
                .unwrap_or_else(|| std::format!("#{i}"))
        };
        let Some(reason) = ours.diff(theirs, &path, &name, local, remote) else {
            return true;
        };
        match self.role {
            Role::Client => self.close(reason),
            Role::Server => self.reject_peer(peer, reason, now_us),
        }
        false
    }

    /// Records the current values of one part of entity `id` (`(field index, value)`); see
    /// [`Replication::record_part`]. Returns true when a value changed.
    pub fn record_part(&mut self, id: NetId, part: u16, values: &[(u16, Value)]) -> bool {
        let Some(scene) = self.entities.get(id).map(|r| r.scene) else {
            return false;
        };
        let tick = self.clock.tick();
        self.repl.record_part(id, scene, part, values, tick)
    }

    /// Decodes a `Create`'s initial state with this side's property table of `scene`: `(part, field,
    /// value)`. `None` when the table is not registered or the bytes do not decode.
    pub fn decode_state(&self, scene: u64, bytes: &[u8]) -> Option<Vec<(u16, u16, Value)>> {
        self.repl.decode_state(scene, bytes)
    }

    /// The property values snapshots delivered since the last call, in arrival order. WP4: the hot
    /// fields of entities this side predicts are not among them (they go to the prediction's
    /// confirmed mirror; see [`Session::take_rollback`]).
    pub fn drain_state_updates(&mut self) -> Vec<StateUpdate> {
        self.route_updates();
        self.pass_updates.drain(..).collect()
    }

    pub fn set_entity_priority(&mut self, id: NetId, priority: f32) {
        self.repl.set_priority(id, priority);
    }

    /// The hook WP14 scores (2.7): `id` is interacting with `peer`.
    pub fn set_interacting(&mut self, peer: PeerId, id: NetId, on: bool) {
        self.repl.set_interacting(peer, id, on);
    }

    pub fn set_budget(&mut self, bytes_per_s: u32) {
        self.budget_bytes_per_s = bytes_per_s.max(1);
    }

    pub fn budget(&self) -> u32 {
        self.budget_bytes_per_s
    }

    pub fn repl_stats(&self) -> ReplStats {
        self.repl.stats
    }

    pub fn replication(&self) -> &Replication {
        &self.repl
    }

    /// 2.2 step 6 for state: the server builds each joined peer's snapshot (and reliable fields)
    /// within its budget; a client sends its `Ack` when snapshots arrived. Call once per tick, after
    /// recording.
    pub fn send_snapshots(&mut self, now_us: u64) {
        if !self.is_connected() {
            return;
        }
        match self.role {
            Role::Client => {
                if let Some(ack) = self.repl.take_ack() {
                    self.send(SERVER, &Message::Ack(ack), now_us);
                }
                if let Some(msg) = self.owner_inputs.take_msg() {
                    self.input_stats.messages_sent += 1;
                    self.send(SERVER, &Message::Input(input::encode_msg(&msg)), now_us);
                }
                self.send_owner_states(now_us);
            }
            Role::Server => {
                let tick = self.clock.tick();
                let rate = self.tick_rate();
                let budget = self.budget_bytes_per_s;
                for peer in self.peers.joined() {
                    let entities = &self.entities;
                    let owner_of = |id: NetId| entities.get(id).map(|r| r.owner);
                    // WP7: a `client_owned` entity's state comes from its owner, who never gets it
                    // back (a client's own spawn is one).
                    let skip = |id: NetId| {
                        entities
                            .get(id)
                            .is_none_or(|r| r.client_owned && r.owner == peer)
                    };
                    let out = self
                        .repl
                        .build(peer, tick, tick, rate, budget, &owner_of, &skip);
                    for f in out.fields {
                        self.send(peer, &Message::Field(f), now_us);
                    }
                    if let Some(body) = out.snapshot {
                        self.send(peer, &Message::Snapshot(body), now_us);
                    }
                }
                self.send_hashes(tick, now_us);
            }
        }
    }
}

/// Input and prediction (WP4, docs/PLAN_NETCODE.md 2.1 "Input", 2.5).
///
/// The tick mapping: a client predicts the server's future. Its local tick follows the server's (set at
/// `Welcome`, the offset estimated by `Time`); the *predicted tick* is `local tick + lead`, where the lead
/// covers half the round trip plus jitter and a margin ([`predict::lead_target`]). The owner simulates
/// predicted tick P at its local tick, records its hot state for P, and stamps the frame it samples at
/// that moment with `P + input_delay`, so the frame reaches the server before the server simulates that
/// tick. A snapshot of server tick T is then compared with the owner's history at T. On the server the
/// predicted tick is its own tick.
impl Session {
    fn route_updates(&mut self) {
        while let Some(u) = self.repl.poll_update() {
            if self.role == Role::Client {
                // WP7: the owner's own state never comes back (a late snapshot after a transfer).
                if self.owns_state(u.id) {
                    continue;
                }
                let key = (u.id, u.part, u.field);
                self.auth.pending_writes.remove(&key);
                self.auth.rejects.remove(&key);
                self.auth.received.insert(key, u.value.clone());
                self.note_hash(u.id, u.part, u.field, &u.value, u.tick);
            }
            if self.prediction.diverts(u.id, u.part, u.field) {
                self.prediction
                    .confirm(u.id, u.part, u.field, u.value, u.tick);
            } else if let Some(desc) = self.interpolated_desc(u.id, u.part, u.field) {
                // WP5: an interpolated field of a replica goes to its sample buffer; gdnet writes the
                // sampled value each render frame (`interp_frame`).
                self.interp
                    .push((u.id, u.part, u.field), &desc, u.tick, u.value);
            } else {
                self.pass_updates.push_back(u);
            }
        }
        if self.hash.on {
            self.hash.rx.commit();
        }
    }

    /// WP5: the descriptor of a field this client interpolates: an `interpolate` field of an entity it
    /// does not predict.
    fn interpolated_desc(&self, id: NetId, part: u16, field: u16) -> Option<PropDesc> {
        if self.role != Role::Client || self.prediction.is_tracked(id) || self.owns_state(id) {
            return None;
        }
        let scene = self.entities.get(id)?.scene;
        let d = self.repl.table(scene)?.get(part, field)?;
        d.interpolate.then(|| d.clone())
    }

    fn forget_input(&mut self, id: NetId) {
        self.hash.rx.forget(id, false);
        self.auth.received.retain(|k, _| k.0 != id);
        self.auth.pending_writes.retain(|k| k.0 != id);
        self.auth.rejects.retain(|k, _| k.0 != id);
        self.auth.owner_tick.remove(&id);
        self.auth.last_sent.remove(&id);
        self.interp.reset(id);
        self.prediction.untrack(id);
        self.owner_inputs.forget(id);
        self.input_buffers.remove(&id);
        self.server_inputs.remove(&id);
    }

    fn on_input(&mut self, peer: PeerId, body: &[u8], now_tick: u64) {
        let Some(msg) = input::decode_msg(body) else {
            self.stats.decode_errors += 1;
            return;
        };
        self.input_stats.messages_received += 1;
        for (id, frames) in msg.entities {
            if self.entities.get(id).is_none_or(|r| r.owner != peer) {
                self.input_stats.refused += 1;
                continue;
            }
            let buf = self.input_buffers.entry(id).or_default();
            for (k, f) in frames.into_iter().enumerate() {
                let Some(t) = msg.tick.checked_sub(k as u64) else {
                    break;
                };
                self.input_stats.frames_received += 1;
                buf.insert(t, f, now_tick);
            }
        }
    }

    /// Per tick: the server takes this tick's frame for every client-owned entity; a client updates its
    /// lead and the adaptive delay.
    fn input_tick(&mut self, tick: u64) {
        match self.role {
            Role::Server => {
                self.server_inputs.clear();
                let owned: Vec<NetId> = self
                    .entities
                    .iter()
                    .filter(|r| r.owner != SERVER && r.owner != 0)
                    .map(|r| r.id)
                    .collect();
                self.input_buffers.retain(|id, _| owned.contains(id));
                for id in owned {
                    let taken = self.input_buffers.entry(id).or_default().take(tick);
                    if taken.repeated {
                        self.input_stats.repeated += 1;
                    }
                    self.server_inputs.insert(id, taken);
                }
            }
            Role::Client => {
                let tick_ms = self.clock.tick_us() / 1000.0;
                let Some(state) = self.peers.get(SERVER) else {
                    return;
                };
                let (Some(rtt), Some(jitter)) = (state.rtt.rtt_ms(), state.rtt.jitter_ms()) else {
                    return;
                };
                let rtt_ticks = rtt / tick_ms;
                let target = predict::lead_target(self.clock.offset(), rtt_ticks, jitter / tick_ms);
                self.lead = Some(predict::next_lead(self.lead, target));
                let interacting = self.interaction;
                if let Some(a) = self.adaptive.as_mut() {
                    a.set_interacting(interacting);
                    a.step((rtt_ticks / 2.0).ceil());
                }
            }
        }
    }

    /// The server tick this side simulates now: a client's predicted tick, the server's own tick.
    pub fn predicted_tick(&self) -> u64 {
        let tick = self.clock.tick();
        match self.role {
            Role::Server => tick,
            Role::Client => {
                let lead = self.lead.unwrap_or(predict::LEAD_MARGIN as i64);
                (tick as i64 + lead).max(0) as u64
            }
        }
    }

    /// The client's prediction lead in ticks (0 on the server; None before the round trip is known).
    pub fn prediction_lead(&self) -> Option<i64> {
        match self.role {
            Role::Server => Some(0),
            Role::Client => self.lead,
        }
    }

    /// `Net.input_delay_ticks`: frames sampled now apply this many ticks later.
    pub fn set_input_delay(&mut self, ticks: u32) {
        self.input_delay = ticks.min(60);
    }

    /// `Net.adaptive_delay` (experimental): on adds the adaptive controller.
    pub fn set_adaptive_delay(&mut self, on: bool) {
        match (on, self.adaptive.is_some()) {
            (true, false) => self.adaptive = Some(AdaptiveDelay::default()),
            (false, true) => self.adaptive = None,
            _ => {}
        }
    }

    /// The hook the adaptive delay ramps on (WP9 and WP14 may drive it): this peer is interacting
    /// (with a contested object, an opponent). Distinct from [`Session::set_interacting`], which is the
    /// snapshot priority hook.
    pub fn set_interaction(&mut self, on: bool) {
        self.interaction = on;
    }

    /// The effective input delay in ticks (the fixed delay plus the adaptive part).
    pub fn input_delay(&self) -> u32 {
        self.input_delay + self.adaptive.as_ref().map_or(0, AdaptiveDelay::ticks)
    }

    pub fn adaptive_delay(&self) -> Option<&AdaptiveDelay> {
        self.adaptive.as_ref()
    }

    /// The owner sampled `frame` for entity `id` this tick: stored for the tick it applies to
    /// (`predicted_tick() + input_delay()`, returned) and sent with the next packets.
    pub fn local_input(&mut self, id: NetId, frame: InputFrame) -> u64 {
        let t = self.predicted_tick() + u64::from(self.input_delay());
        self.owner_inputs.store(id, t, frame);
        t
    }

    /// The owner's frame of `id` for `tick` (the tick it applies to).
    pub fn owner_frame(&self, id: NetId, tick: u64) -> Option<&InputFrame> {
        self.owner_inputs.get(id, tick)
    }

    /// Server: the frame client-owned entity `id` uses this tick.
    pub fn server_input(&self, id: NetId) -> Option<&TakenFrame> {
        self.server_inputs.get(&id)
    }

    /// Owner: start (or restart with an empty history) predicting `id` under `policy`. `initial`
    /// seeds the confirmed state (the `Create`'s decoded values). False for an unknown entity or scene.
    pub fn track_prediction(
        &mut self,
        id: NetId,
        policy: Policy,
        initial: &[(u16, u16, Value)],
    ) -> bool {
        let Some(scene) = self.entities.get(id).map(|r| r.scene) else {
            return false;
        };
        let Some(table) = self.repl.table(scene) else {
            return false;
        };
        let table = table.clone();
        self.prediction.track(id, &table, policy, initial);
        true
    }

    pub fn untrack_prediction(&mut self, id: NetId) {
        self.prediction.untrack(id);
        self.owner_inputs.forget(id);
    }

    pub fn is_predicted(&self, id: NetId) -> bool {
        self.prediction.is_tracked(id)
    }

    pub fn set_prediction_policy(&mut self, id: NetId, policy: Policy) {
        self.prediction.set_policy(id, policy);
    }

    /// Owner: the hot state of `id` after simulating predicted tick `tick` (values in
    /// [`Prediction::hot_fields`] order).
    pub fn record_prediction(&mut self, id: NetId, tick: u64, values: Vec<Value>) {
        self.prediction.record(id, tick, values);
    }

    pub fn prediction(&self) -> &Prediction {
        &self.prediction
    }

    pub fn set_max_rollback_ticks(&mut self, n: u32) {
        self.prediction.set_max_ticks(n);
        self.owner_inputs.set_keep(u64::from(n.max(1)) + 16);
    }

    /// The rollback the confirmations of this tick ask for, if any (call once per tick, after the
    /// packets were processed).
    pub fn take_rollback(&mut self) -> Option<Rollback> {
        self.route_updates();
        self.prediction.evaluate()
    }

    pub fn predict_stats(&self) -> PredictStats {
        self.prediction.stats.clone()
    }

    pub fn input_stats(&self) -> InputStats {
        self.input_stats
    }
}

/// Authority (WP7, docs/PLAN_NETCODE.md 2.6; see [`crate::authority`]): ownership requests and
/// releases, `client_owned` state from owners, rejected writes.
impl Session {
    pub fn authority_stats(&self) -> AuthorityStats {
        self.auth.stats
    }

    /// True when this side owns `id` and its state is this side's to send (a `client_owned` entity it
    /// owns, on a client).
    pub fn owns_state(&self, id: NetId) -> bool {
        self.role == Role::Client
            && self
                .entities
                .get(id)
                .is_some_and(|r| r.client_owned && r.owner == self.local_id)
    }

    /// Asks for ownership of `id`. A client sends `RequestOwner` (the outcome arrives as
    /// `EntityEvent::OwnerReply`); on the server the request is decided at once for the server itself
    /// (a `request` entity owned by a client asks that client first).
    pub fn request_owner(&mut self, id: NetId, now_us: u64) -> Result<(), String> {
        if !self.is_connected() {
            return Err(String::from("the session is not connected"));
        }
        if !self.entities.contains(id) {
            return Err(std::format!("entity {id} is not spawned on this peer"));
        }
        match self.role {
            Role::Server => self.handle_request(id, SERVER, now_us),
            Role::Client => self.send(
                SERVER,
                &Message::Entity(EntityAction::RequestOwner { id }),
                now_us,
            ),
        }
        Ok(())
    }

    /// Gives `id` back to the server. A client must own it; on the server it is `set_owner(id, 1)`.
    pub fn release_owner(&mut self, id: NetId, now_us: u64) -> bool {
        match self.role {
            Role::Server => self.set_owner(id, SERVER, now_us),
            Role::Client => {
                if !self.is_connected()
                    || self
                        .entities
                        .get(id)
                        .is_none_or(|r| r.owner != self.local_id)
                {
                    return false;
                }
                self.send(
                    SERVER,
                    &Message::Entity(EntityAction::ReleaseOwner { id }),
                    now_us,
                );
                true
            }
        }
    }

    /// gdnet's answer to an `EntityEvent::OwnerQuery` (may `requester` have `id`?).
    pub fn answer_owner_query(&mut self, id: NetId, requester: PeerId, grant: bool, now_us: u64) {
        match self.role {
            Role::Server => {
                let asked = self.auth.queries.contains_key(&(id, requester))
                    && self.entities.get(id).is_some_and(|r| r.owner == SERVER);
                if asked {
                    self.finish_query(id, requester, grant, now_us);
                }
            }
            Role::Client => self.send(
                SERVER,
                &Message::Entity(EntityAction::OwnerAnswer {
                    id,
                    requester,
                    grant,
                }),
                now_us,
            ),
        }
    }

    /// Server: a request by `requester` (a client, or the server itself) for `id`.
    fn handle_request(&mut self, id: NetId, requester: PeerId, now_us: u64) {
        let Some((owner, transfer, manager)) = self
            .entities
            .get(id)
            .map(|r| (r.owner, r.transfer, r.parent.manager))
        else {
            self.reply_owner(id, requester, false, now_us);
            return;
        };
        match authority::decide(transfer, owner, requester) {
            Decision::Grant => {
                self.set_owner(id, requester, now_us);
                self.reply_owner(id, requester, true, now_us);
            }
            Decision::Refuse => self.reply_owner(id, requester, false, now_us),
            Decision::Ask(asked) => {
                if self.auth.queries.contains_key(&(id, requester)) {
                    return;
                }
                let tick = self.clock.tick();
                if asked == SERVER {
                    self.auth.queries.insert((id, requester), tick);
                    self.entity_events.push_back(EntityEvent::OwnerQuery {
                        id,
                        manager,
                        requester,
                    });
                } else if self.peers.is_joined(asked) {
                    self.auth.queries.insert((id, requester), tick);
                    self.send(
                        asked,
                        &Message::Entity(EntityAction::OwnerQuery { id, requester }),
                        now_us,
                    );
                } else {
                    self.reply_owner(id, requester, false, now_us);
                }
            }
        }
    }

    /// Server: the owner decided a query.
    fn finish_query(&mut self, id: NetId, requester: PeerId, grant: bool, now_us: u64) {
        if self.auth.queries.remove(&(id, requester)).is_none() {
            return;
        }
        let ok = grant && self.entities.contains(id);
        if ok {
            self.set_owner(id, requester, now_us);
        }
        self.reply_owner(id, requester, ok, now_us);
    }

    /// Server: tells `requester` the outcome (the server's own request: a local event).
    fn reply_owner(&mut self, id: NetId, requester: PeerId, granted: bool, now_us: u64) {
        if granted {
            self.auth.stats.requests_granted += 1;
        } else {
            self.auth.stats.requests_refused += 1;
        }
        if requester == SERVER {
            if let Some(manager) = self.entities.get(id).map(|r| r.parent.manager) {
                self.entity_events.push_back(EntityEvent::OwnerReply {
                    id,
                    manager,
                    granted,
                });
            }
        } else if self.peers.is_joined(requester) {
            self.send(
                requester,
                &Message::Entity(EntityAction::OwnerReply { id, granted }),
                now_us,
            );
        }
    }

    fn expire_queries(&mut self, tick: u64, now_us: u64) {
        if self.auth.queries.is_empty() {
            return;
        }
        let limit = OWNER_QUERY_TIMEOUT_S * u64::from(self.tick_rate());
        let expired: Vec<(NetId, PeerId)> = self
            .auth
            .queries
            .iter()
            .filter(|(_, asked)| tick.saturating_sub(**asked) >= limit)
            .map(|(k, _)| *k)
            .collect();
        for (id, r) in expired {
            self.finish_query(id, r, false, now_us);
        }
    }

    /// Records an encoded full state (a `Create`'s) as this side's values of `id`.
    fn record_encoded(&mut self, id: NetId, scene: u64, bytes: &[u8]) -> Vec<(u16, u16, Value)> {
        let Some(values) = self.repl.decode_state(scene, bytes) else {
            return Vec::new();
        };
        let tick = self.clock.tick();
        let mut by_part: BTreeMap<u16, Vec<(u16, Value)>> = BTreeMap::new();
        for (p, f, v) in &values {
            by_part.entry(*p).or_default().push((*f, v.clone()));
        }
        for (p, vals) in by_part {
            self.repl.record_part(id, scene, p, &vals, tick);
        }
        values
    }

    /// Server: an owner state (unreliable) or a write (reliable) from `peer`.
    fn on_state(&mut self, peer: PeerId, m: StateMsg, reliable: bool, now_us: u64) {
        let tick = self.clock.tick();
        for (id, bytes) in m.entities {
            let Some((owner, client_owned, scene, manager)) = self
                .entities
                .get(id)
                .map(|r| (r.owner, r.client_owned, r.scene, r.parent.manager))
            else {
                continue;
            };
            if client_owned && owner == peer {
                if !reliable && self.auth.owner_tick.get(&id).is_some_and(|t| *t >= m.tick) {
                    continue;
                }
                if !reliable {
                    self.auth.owner_tick.insert(id, m.tick);
                }
                self.auth.stats.owner_states_received += 1;
                for (part, field, value) in self.record_encoded(id, scene, &bytes) {
                    self.pass_updates.push_back(StateUpdate {
                        id,
                        part,
                        field,
                        value,
                        tick,
                    });
                }
                continue;
            }
            // A rejected write: the writer restores the server's values.
            let throttled = !reliable
                && self
                    .auth
                    .reject_tick
                    .get(&(peer, id))
                    .is_some_and(|t| tick.saturating_sub(*t) < u64::from(self.tick_rate() / 2));
            if throttled {
                continue;
            }
            self.auth.reject_tick.insert((peer, id), tick);
            let fields: Vec<(u16, u16)> = self
                .repl
                .decode_state(scene, &bytes)
                .unwrap_or_default()
                .into_iter()
                .map(|(p, f, _)| (p, f))
                .collect();
            self.auth.stats.writes_rejected += 1;
            self.entity_events.push_back(EntityEvent::WriteRejected {
                id,
                manager,
                peer,
                restore: Vec::new(),
            });
            self.send(
                peer,
                &Message::WriteReject(WriteReject { id, fields }),
                now_us,
            );
        }
    }

    /// Client: the server refused this side's write.
    fn on_write_reject(&mut self, r: WriteReject) {
        let Some(manager) = self.entities.get(r.id).map(|e| e.parent.manager) else {
            return;
        };
        self.auth.stats.writes_rejected += 1;
        let mut restore = Vec::new();
        for (p, f) in r.fields {
            self.auth.pending_writes.remove(&(r.id, p, f));
            *self.auth.rejects.entry((r.id, p, f)).or_default() += 1;
            if let Some(v) = self.auth.received.get(&(r.id, p, f)) {
                restore.push((p, f, v.clone()));
            }
        }
        self.entity_events.push_back(EntityEvent::WriteRejected {
            id: r.id,
            manager,
            peer: self.local_id,
            restore,
        });
    }

    /// Client: the values the server sent for `id` outside snapshots (a `Create`'s initial state), the
    /// baseline of write detection.
    pub fn remember_received(&mut self, id: NetId, values: &[(u16, u16, Value)]) {
        if self.role != Role::Client {
            return;
        }
        for (p, f, v) in values {
            self.auth.received.insert((id, *p, *f), v.clone());
            self.note_hash(id, *p, *f, v, 0);
        }
        if self.hash.on {
            self.hash.rx.commit();
        }
    }

    /// Client: compares the polled values of one part of `id`, an entity this side does not own, with
    /// the last values the server sent; each [`authority::write_checked`] field that differs (and is
    /// not already waiting for its answer) is a write, queued for the server, which rejects it.
    /// Returns the number of fields written.
    pub fn check_writes(&mut self, id: NetId, part: u16, values: &[(u16, Value)]) -> usize {
        if self.role != Role::Client || !self.is_connected() {
            return 0;
        }
        let Some((owner, scene)) = self.entities.get(id).map(|r| (r.owner, r.scene)) else {
            return 0;
        };
        if owner == self.local_id {
            return 0;
        }
        let Some(table) = self.repl.table(scene) else {
            return 0;
        };
        let mut written = Vec::new();
        for (f, v) in values {
            let Some(d) = table.get(part, *f) else {
                continue;
            };
            if !authority::write_checked(d) {
                continue;
            }
            let key = (id, part, *f);
            if self.auth.pending_writes.contains(&key)
                || self
                    .auth
                    .rejects
                    .get(&key)
                    .is_some_and(|n| *n >= MAX_REJECTS_IN_A_ROW)
            {
                continue;
            }
            let Some(had) = self.auth.received.get(&key) else {
                continue;
            };
            if predict::canonical(d.ty, d.quant, had) != predict::canonical(d.ty, d.quant, v) {
                written.push((part, *f, v.clone()));
            }
        }
        if written.is_empty() {
            return 0;
        }
        for (p, f, _) in &written {
            self.auth.pending_writes.insert((id, *p, *f));
        }
        let bytes = self.repl.encode_values(id, scene, &written);
        self.auth.write_queue.push((id, bytes));
        self.auth.stats.writes_sent += written.len() as u64;
        written.len()
    }

    /// Client post-tick: the state of every `client_owned` entity this side owns that changed in the
    /// last few ticks (or is due for its keyframe), packed into unreliable owner-state messages; then
    /// the queued writes, reliably.
    fn send_owner_states(&mut self, now_us: u64) {
        let tick = self.clock.tick();
        let rate = u64::from(self.tick_rate());
        let local = self.local_id;
        let owned: Vec<NetId> = self
            .entities
            .iter()
            .filter(|r| r.client_owned && r.owner == local)
            .map(|r| r.id)
            .collect();
        let mut msg = StateMsg {
            tick,
            entities: Vec::new(),
        };
        let mut size = 0usize;
        for id in owned {
            let Some(changed) = self.repl.last_change(id) else {
                continue;
            };
            let last = self.auth.last_sent.get(&id).copied();
            let recent = changed + authority::OWNER_STATE_REDUNDANCY >= tick;
            let keyframe = last.is_none_or(|l| tick.saturating_sub(l) >= rate);
            if !recent && !keyframe {
                continue;
            }
            let bytes = self.repl.encode_full(id);
            if bytes.is_empty() {
                continue;
            }
            if size + bytes.len() > authority::OWNER_STATE_MAX_BYTES && !msg.entities.is_empty() {
                let full = core::mem::replace(
                    &mut msg,
                    StateMsg {
                        tick,
                        entities: Vec::new(),
                    },
                );
                self.auth.stats.owner_states_sent += 1;
                self.send(SERVER, &Message::OwnerState(full), now_us);
                size = 0;
            }
            size += bytes.len() + 8;
            msg.entities.push((id, bytes));
            self.auth.last_sent.insert(id, tick);
        }
        if !msg.entities.is_empty() {
            self.auth.stats.owner_states_sent += 1;
            self.send(SERVER, &Message::OwnerState(msg), now_us);
        }
        if !self.auth.write_queue.is_empty() {
            let entities = core::mem::take(&mut self.auth.write_queue);
            self.send(SERVER, &Message::Write(StateMsg { tick, entities }), now_us);
        }
    }
}

/// Keyed state (WP7, docs/PLAN_NETCODE.md 2.4; see [`crate::keyed`]).
impl Session {
    pub fn keyed(&self) -> &KeyedState {
        &self.keyed
    }

    /// The next change of the keyed map (remote or local) for gdnet's signals.
    pub fn poll_key_event(&mut self) -> Option<KeyEvent> {
        self.keyed.poll_event()
    }

    fn send_keyed(&mut self, peer: PeerId, msg: KeyMsg, now_us: u64) {
        let m = Message::Keyed(msg);
        self.keyed.stats.messages_sent += 1;
        self.keyed.stats.bytes_sent += wire::encode(&m).len() as u64;
        self.send(peer, &m, now_us);
    }

    /// Server: sends a set or erase of `path` to every joined peer that sees it (but `except`).
    fn share_key(&mut self, path: &str, except: Option<PeerId>, now_us: u64) {
        let value = self.keyed.get(path).cloned();
        let peers: Vec<PeerId> = self
            .peers
            .joined()
            .into_iter()
            .filter(|p| Some(*p) != except && self.keyed.visible(*p, path))
            .collect();
        for peer in peers {
            let msg = match &value {
                Some(v) => KeyMsg::Set {
                    path: String::from(path),
                    value: v.clone(),
                },
                None => KeyMsg::Erase {
                    path: String::from(path),
                },
            };
            self.send_keyed(peer, msg, now_us);
        }
    }

    fn refuse_key_locally(&self, path: &str) -> Result<(), String> {
        if self.role == Role::Client && !self.keyed.may_write(self.local_id, path) {
            return Err(std::format!(
                "this peer may not write the keyed state path {path:?}: only the server writes it (the server opens paths with Net.state.allow(prefix, who))"
            ));
        }
        Ok(())
    }

    /// Sets `path` to `value` (encoded bytes). The server applies and shares it; a client allowed to
    /// write the path applies it at once and sends it to the server (which may still refuse it); a
    /// client without the permission gets an error and nothing is sent.
    pub fn keyed_set(&mut self, path: &str, value: Vec<u8>, now_us: u64) -> Result<(), String> {
        self.refuse_key_locally(path)?;
        match self.role {
            Role::Server => {
                if self.keyed.apply_set(path, value) {
                    self.share_key(path, None, now_us);
                }
            }
            Role::Client => {
                if !self.is_connected() {
                    return Err(String::from("the session is not connected"));
                }
                self.keyed.apply_set(path, value.clone());
                let msg = KeyMsg::Set {
                    path: String::from(path),
                    value,
                };
                self.send_keyed(SERVER, msg, now_us);
            }
        }
        Ok(())
    }

    /// Erases `path` (the same rules as [`Session::keyed_set`]).
    pub fn keyed_erase(&mut self, path: &str, now_us: u64) -> Result<(), String> {
        self.refuse_key_locally(path)?;
        match self.role {
            Role::Server => {
                if self.keyed.get(path).is_some() {
                    let visible: Vec<PeerId> = self
                        .peers
                        .joined()
                        .into_iter()
                        .filter(|p| self.keyed.visible(*p, path))
                        .collect();
                    self.keyed.apply_erase(path);
                    for peer in visible {
                        let msg = KeyMsg::Erase {
                            path: String::from(path),
                        };
                        self.send_keyed(peer, msg, now_us);
                    }
                }
            }
            Role::Client => {
                if !self.is_connected() {
                    return Err(String::from("the session is not connected"));
                }
                self.keyed.apply_erase(path);
                let msg = KeyMsg::Erase {
                    path: String::from(path),
                };
                self.send_keyed(SERVER, msg, now_us);
            }
        }
        Ok(())
    }

    /// Server: lets `who` write the paths under `prefix`; the rules go to every client.
    pub fn keyed_allow(&mut self, prefix: &str, who: Who, now_us: u64) {
        if self.role != Role::Server {
            return;
        }
        self.keyed.allow(prefix, who);
        let rules = self.keyed.rules().to_vec();
        for peer in self.peers.joined() {
            let msg = KeyMsg::Rules {
                rules: rules.clone(),
            };
            self.send_keyed(peer, msg, now_us);
        }
    }

    /// Server: a view over `prefix`, with the decisions gdnet's filter made for the joined peers.
    /// Peers that saw paths under it and may not any more get erases; peers that may get the paths.
    pub fn keyed_view(&mut self, prefix: &str, decisions: &[(PeerId, bool)], now_us: u64) {
        if self.role != Role::Server {
            return;
        }
        let entries = self.keyed.entries_under(prefix);
        let peers = self.peers.joined();
        let before: Vec<Vec<bool>> = peers
            .iter()
            .map(|p| {
                entries
                    .iter()
                    .map(|(k, _)| self.keyed.visible(*p, k))
                    .collect()
            })
            .collect();
        self.keyed.add_view(prefix);
        for (peer, vis) in decisions {
            self.keyed.set_visible(prefix, *peer, *vis);
        }
        for (i, peer) in peers.iter().enumerate() {
            self.send_visibility_diff(*peer, &entries, &before[i], now_us);
        }
    }

    /// Server: `peer`'s decision for the view `prefix` (gdnet re-evaluates the filter every tick).
    pub fn keyed_set_visible(&mut self, prefix: &str, peer: PeerId, visible: bool, now_us: u64) {
        if self.role != Role::Server || !self.keyed.has_view(prefix) {
            return;
        }
        let entries = self.keyed.entries_under(prefix);
        let before: Vec<bool> = entries
            .iter()
            .map(|(k, _)| self.keyed.visible(peer, k))
            .collect();
        if self.keyed.set_visible(prefix, peer, visible) && self.peers.is_joined(peer) {
            self.send_visibility_diff(peer, &entries, &before, now_us);
        }
    }

    fn send_visibility_diff(
        &mut self,
        peer: PeerId,
        entries: &[(String, Vec<u8>)],
        before: &[bool],
        now_us: u64,
    ) {
        for ((k, v), was) in entries.iter().zip(before) {
            let now = self.keyed.visible(peer, k);
            if now == *was {
                continue;
            }
            let msg = if now {
                KeyMsg::Set {
                    path: k.clone(),
                    value: v.clone(),
                }
            } else {
                KeyMsg::Erase { path: k.clone() }
            };
            self.send_keyed(peer, msg, now_us);
        }
    }

    fn on_keyed(&mut self, peer: PeerId, m: KeyMsg, now_us: u64) {
        match self.role {
            Role::Server => {
                if !self.peers.is_joined(peer) {
                    return;
                }
                let path = match &m {
                    KeyMsg::Set { path, .. } | KeyMsg::Erase { path } => path.clone(),
                    _ => return,
                };
                if !self.keyed.may_write(peer, &path) {
                    self.keyed.stats.rejected += 1;
                    let value = self.keyed.get(&path).cloned();
                    self.send_keyed(peer, KeyMsg::Rejected { path, value }, now_us);
                    return;
                }
                let changed = match m {
                    KeyMsg::Set { path, value } => self.keyed.apply_set(&path, value),
                    KeyMsg::Erase { path } => {
                        let visible: Vec<PeerId> = self
                            .peers
                            .joined()
                            .into_iter()
                            .filter(|p| *p != peer && self.keyed.visible(*p, &path))
                            .collect();
                        if self.keyed.apply_erase(&path) {
                            for p in visible {
                                let msg = KeyMsg::Erase { path: path.clone() };
                                self.send_keyed(p, msg, now_us);
                            }
                        }
                        false
                    }
                    _ => false,
                };
                if changed {
                    // The writer applied it already; everyone else who sees it gets it.
                    self.share_key(&path, Some(peer), now_us);
                }
            }
            Role::Client => {
                if peer != SERVER {
                    return;
                }
                match m {
                    KeyMsg::Set { path, value } => {
                        self.keyed.apply_set(&path, value);
                    }
                    KeyMsg::Erase { path } => {
                        self.keyed.apply_erase(&path);
                    }
                    KeyMsg::Full { entries, rules } => {
                        self.keyed.set_rules(rules);
                        self.keyed.apply_full(entries);
                    }
                    KeyMsg::Rules { rules } => self.keyed.set_rules(rules),
                    KeyMsg::Rejected { path, value } => {
                        match value {
                            Some(v) => self.keyed.apply_set(&path, v),
                            None => self.keyed.apply_erase(&path),
                        };
                        self.keyed.stats.rejected += 1;
                        self.keyed.push_event(KeyEvent::Rejected(path));
                    }
                }
            }
        }
    }
}

/// Interpolation and smoothing (WP5, docs/PLAN_NETCODE.md 2.2 step 7, 2.5; see [`crate::interp`]).
impl Session {
    /// Sets the jitter buffer's starting value (1..=6 ticks), whether it adapts, and the default
    /// teleport distance (0 = automatic).
    pub fn set_interpolation(&mut self, buffer_ticks: u32, adaptive: bool, teleport: f64) {
        self.interp.timeline.configure(buffer_ticks, adaptive);
        self.interp.set_default_teleport(teleport);
    }

    pub fn interp(&self) -> &Interp {
        &self.interp
    }

    /// One render frame (2.2 step 7): advances the render timeline, decays the offsets and returns
    /// the replica fields whose displayed value changed, for gdnet to write.
    pub fn interp_frame(&mut self, now_us: u64) -> Vec<((NetId, u16, u16), Value)> {
        if self.role != Role::Client {
            return Vec::new();
        }
        self.route_updates();
        self.interp.frame(now_us)
    }

    /// A replica's initial value of an interpolated field (from its `Create`): held until the first
    /// snapshot sample, so the first movement starts from it.
    pub fn interp_seed(&mut self, id: NetId, part: u16, field: u16, value: Value) {
        if let Some(desc) = self.interpolated_desc(id, part, field) {
            self.interp.seed((id, part, field), &desc, value);
        }
    }

    /// Whether this client interpolates the field (it is a replica's `interpolate` field).
    pub fn is_interpolated(&self, id: NetId, part: u16, field: u16) -> bool {
        self.interpolated_desc(id, part, field).is_some()
    }

    /// After a rollback of an owned entity: keeps `displayed - replayed` as the field's owned offset
    /// (see [`Interp::blend_owned`]). Only for `interpolate` fields.
    pub fn blend_owned(
        &mut self,
        id: NetId,
        part: u16,
        field: u16,
        displayed: &Value,
        replayed: &Value,
    ) -> bool {
        let Some(scene) = self.entities.get(id).map(|r| r.scene) else {
            return false;
        };
        let Some(desc) = self
            .repl
            .table(scene)
            .and_then(|t| t.get(part, field))
            .cloned()
        else {
            return false;
        };
        if !desc.interpolate {
            return false;
        }
        self.interp
            .blend_owned((id, part, field), &desc, displayed, replayed)
    }

    /// The owned fields that have an offset to show.
    pub fn owned_offset_keys(&self) -> Vec<(NetId, u16, u16)> {
        self.interp.owned_keys()
    }

    /// `sim` with the field's owned offset added (None without an offset).
    pub fn apply_owned_offset(
        &self,
        id: NetId,
        part: u16,
        field: u16,
        sim: &Value,
    ) -> Option<Value> {
        self.interp
            .owned_offset((id, part, field))
            .map(|o| o.apply(sim))
    }

    /// Forgets the samples and offsets of `id` (its authority changed: WP7 calls this too).
    pub fn interp_reset(&mut self, id: NetId) {
        self.interp.reset(id);
    }
}

/// Hash telemetry (WP8, docs/PLAN_NETCODE.md 2.4 `Hash`; see hash.rs).
impl Session {
    /// `gdnet/debug/hash_telemetry` on or off, and the entities sampled per tick (server).
    pub fn set_hash_telemetry(&mut self, on: bool, sample: u32) {
        self.hash.on = on;
        self.hash.sample = sample;
    }

    /// Server: an entity whose hash did not change since a peer last got it is sent again after this
    /// many seconds (default [`crate::hash::REFRESH_S`]; 0 = every time it is sampled).
    pub fn set_hash_refresh(&mut self, seconds: f64) {
        let ticks = (seconds.max(0.0) * f64::from(self.tick_rate())).round() as u64;
        self.hash.refresh_ticks = ticks.max(1);
    }

    pub fn hash_telemetry(&self) -> bool {
        self.hash.on
    }

    pub fn hash_stats(&self) -> HashStats {
        HashStats {
            sent: self.hash.sent,
            ..self.hash.rx.stats
        }
    }

    /// Divergences counted for `id` on this client.
    pub fn divergences_of(&self, id: NetId) -> u64 {
        self.hash.rx.divergences_of(id)
    }

    /// The hash of every entity's hot state at server tick `tick`, entities in id order: on the server
    /// its recorded state, for the current tick only; on a client what it holds as of `tick` (None when
    /// an entity's state at `tick` is unknown). For the sync-test mode (WP12).
    pub fn state_hash(&self, tick: u64) -> Option<u32> {
        match self.role {
            Role::Server => {
                if tick != self.clock.tick() {
                    return None;
                }
                Some(
                    self.repl
                        .entity_ids()
                        .into_iter()
                        .fold(crate::hash::FNV_OFFSET, |h, id| {
                            match self.repl.entity_hash(id) {
                                Some(e) => crate::hash::fold_entity(h, id, e),
                                None => h,
                            }
                        }),
                )
            }
            Role::Client => {
                let mut h = crate::hash::FNV_OFFSET;
                for id in self.hash.rx.ids() {
                    h = crate::hash::fold_entity(h, id, self.hash.rx.hash_at(id, tick)?);
                }
                Some(h)
            }
        }
    }

    /// Test hook (the integration tests' deliberate client-side state edit): replaces this client's
    /// held copy of a hot field, as if a value had been applied wrongly. The next comparison of the
    /// entity counts a divergence.
    pub fn debug_corrupt_held(&mut self, id: NetId, part: u16, field: u16, value: &Value) {
        if self.role != Role::Client {
            return;
        }
        let tick = self.hash.rx.last_applied().unwrap_or(0);
        self.note_hash(id, part, field, value, tick);
        self.hash.rx.commit();
    }

    /// Client: a hot field of `id` the server sent, for the hashes.
    fn note_hash(&mut self, id: NetId, part: u16, field: u16, value: &Value, tick: u64) {
        if !self.hash.on || self.role != Role::Client {
            return;
        }
        let Some(scene) = self.entities.get(id).map(|r| r.scene) else {
            return;
        };
        let Some(d) = self.repl.table(scene).and_then(|t| t.get(part, field)) else {
            return;
        };
        if d.schedule != crate::props::Schedule::Hot {
            return;
        }
        let mut w = crate::quant::BitWriter::new();
        crate::quant::encode(&mut w, d.ty, d.quant, value);
        let enc = crate::quant::Encoded::of(&w);
        if Replication::hashed(d, &enc) {
            self.hash.rx.note(id, part, field, enc, tick);
        }
    }

    /// Server, after the snapshots of `tick`: each joined peer gets the hashes of the sampled entities
    /// it can check at this tick, plus every entity it owns when it owns few.
    fn send_hashes(&mut self, tick: u64, now_us: u64) {
        if !self.hash.on {
            return;
        }
        let ids = self.repl.entity_ids();
        let sampled = self.hash.tx.sample(&ids, self.hash.sample as usize);
        let refresh = if self.hash.refresh_ticks == 0 {
            crate::hash::REFRESH_S * u64::from(self.tick_rate())
        } else {
            self.hash.refresh_ticks
        };
        if tick.is_multiple_of(600) {
            let entities = &self.entities;
            let joined = self.peers.joined();
            self.hash
                .last
                .retain(|(p, id), _| joined.contains(p) && entities.get(*id).is_some());
        }
        for peer in self.peers.joined() {
            let mut pick: Vec<NetId> = sampled.clone();
            let owned: Vec<NetId> = self
                .entities
                .iter()
                .filter(|r| r.owner == peer)
                .map(|r| r.id)
                .collect();
            if owned.len() <= crate::hash::OWNED_EVERY_TICK_MAX {
                pick.extend(owned);
            }
            pick.sort_unstable();
            pick.dedup();
            let mut msg = HashMsg {
                tick,
                ..HashMsg::default()
            };
            for id in pick {
                // A `client_owned` entity's owner never receives its state.
                if self
                    .entities
                    .get(id)
                    .is_none_or(|r| r.client_owned && r.owner == peer)
                {
                    continue;
                }
                let (Some(carried), Some(h)) = (
                    self.repl.hash_eligibility(peer, id, tick),
                    self.repl.entity_hash(id),
                ) else {
                    continue;
                };
                // Unchanged since the peer last got it: only after the refresh period.
                if self
                    .hash
                    .last
                    .get(&(peer, id))
                    .is_some_and(|(lh, lt)| *lh == h && tick.saturating_sub(*lt) < refresh)
                {
                    continue;
                }
                self.hash.last.insert((peer, id), (h, tick));
                if carried {
                    msg.carried.push((id, h));
                } else {
                    msg.settled.push((id, h));
                }
            }
            if !msg.is_empty() {
                self.hash.sent += msg.len() as u64;
                self.send(peer, &Message::Hash(msg), now_us);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Channel;

    const TICK_US: u64 = 16_667;

    /// Server and one client (transport id `id`) wired back to back with an instant transport.
    struct Pair {
        server: Session,
        client: Session,
        id: PeerId,
        now: u64,
        server_events: Vec<Event>,
        client_events: Vec<Event>,
    }

    impl Pair {
        fn new(server_cfg: SessionConfig, client_cfg: SessionConfig, id: PeerId) -> Self {
            Self::with(Session::server(server_cfg), Session::client(client_cfg), id)
        }

        fn with(server: Session, client: Session, id: PeerId) -> Self {
            let mut p = Self {
                server,
                client,
                id,
                now: 1_000_000,
                server_events: Vec::new(),
                client_events: Vec::new(),
            };
            p.server.peer_connected(id, p.now);
            p.client.peer_connected(SERVER, p.now);
            p
        }

        fn step(&mut self) {
            self.now += TICK_US;
            self.server.tick(self.now);
            self.client.tick(self.now);
            for o in self.client.outgoing() {
                assert_eq!(o.peer, SERVER);
                self.server.receive(self.id, &o.bytes, self.now);
            }
            for o in self.server.outgoing() {
                assert_eq!(o.peer, self.id);
                self.client.receive(SERVER, &o.bytes, self.now);
            }
            while let Some(e) = self.server.poll_event() {
                self.server_events.push(e);
            }
            while let Some(e) = self.client.poll_event() {
                self.client_events.push(e);
            }
        }
    }

    #[test]
    fn handshake_succeeds() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 7);
        for _ in 0..30 {
            p.step();
        }
        assert!(p.server_events.contains(&Event::Connected {
            local_id: 1,
            tick: 1
        }));
        assert!(p.server_events.contains(&Event::PeerJoined(7)));
        assert!(p
            .client_events
            .iter()
            .any(|e| matches!(e, Event::Connected { local_id: 7, .. })));
        assert!(p.client_events.contains(&Event::PeerJoined(SERVER)));
        assert_eq!(p.server.joined_peers(), vec![7]);
        assert_eq!(p.client.joined_peers(), vec![SERVER]);
        assert_eq!(p.client.local_id(), 7);
        assert!(p.client.is_connected());
        assert_eq!(
            p.client.id_range(),
            IdRange {
                start: ID_RANGE_LEN,
                len: ID_RANGE_LEN
            }
        );
        assert_eq!(p.server.id_range().start, 0);
        // Both have RTT samples; the client's tick follows the server's.
        assert!(p.server.rtt_ms(7).is_some());
        assert!(p.client.rtt_ms(SERVER).is_some());
        assert!(p.client.jitter_ms(SERVER).is_some());
        let diff = p.client.clock().server_tick_estimate() - p.server.current_tick() as f64;
        assert!(diff.abs() <= 1.0, "{diff}");
        assert_eq!(p.server.stats().decode_errors, 0);
    }

    #[test]
    fn protocol_mismatch_names_the_item() {
        let mut salted = SessionConfig::new(60);
        salted.protocol_salt = 99;
        let mut p = Pair::new(SessionConfig::new(60), salted, 3);
        for _ in 0..5 {
            p.step();
        }
        assert_eq!(
            p.client_events,
            vec![Event::Disconnected(String::from(
                "protocol mismatch: protocol salt differs (server 0, client 99)"
            ))]
        );
        assert!(!p.server_events.contains(&Event::PeerJoined(3)));
        assert!(p.client.is_closed());
        // The server asks the transport to drop the peer one second after rejecting it.
        for _ in 0..60 {
            p.step();
        }
        assert!(p
            .server_events
            .iter()
            .any(|e| matches!(e, Event::DisconnectPeer { peer: 3, .. })));
        assert_eq!(p.server.peers().len(), 0);
    }

    #[test]
    fn tick_rate_mismatch_is_named_first() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(30), 3);
        p.step();
        p.step();
        assert_eq!(
            p.client_events,
            vec![Event::Disconnected(String::from(
                "protocol mismatch: tick rate differs (server 60, client 30)"
            ))]
        );
    }

    #[test]
    fn leave_and_loss_of_server() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 4);
        for _ in 0..3 {
            p.step();
        }
        p.server.peer_disconnected(4, p.now);
        assert_eq!(p.server.poll_event(), Some(Event::PeerLeft(4)));
        p.client.peer_disconnected(SERVER, p.now);
        assert_eq!(p.client.poll_event(), Some(Event::PeerLeft(SERVER)));
        assert_eq!(
            p.client.poll_event(),
            Some(Event::Disconnected(String::from(
                "the connection to the server was lost"
            )))
        );
        assert!(!p.client.is_connected());
    }

    #[test]
    fn a_client_ignores_other_clients_connecting() {
        let mut c = Session::client(SessionConfig::new(60));
        c.peer_connected(9, 0);
        assert!(c.outgoing().is_empty());
        assert!(c.peers().is_empty());
        c.peer_connected(SERVER, 0);
        let out = c.outgoing();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, 0);
        assert_eq!(out[0].reliability, Reliability::Reliable);
        assert!(matches!(wire::decode(&out[0].bytes), Ok(Message::Hello(_))));
    }

    #[test]
    fn pings_on_the_diagnostics_channel_at_the_interval() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 2);
        for _ in 0..5 {
            p.step();
        }
        let mut pings = 0;
        for _ in 0..60 {
            p.now += TICK_US;
            p.server.tick(p.now);
            for o in p.server.outgoing() {
                if let Ok(Message::Ping(_)) = wire::decode(&o.bytes) {
                    assert_eq!(o.channel, Channel::Diagnostics.index());
                    assert_eq!(o.reliability, Reliability::Unreliable);
                    pings += 1;
                }
            }
        }
        assert_eq!(pings, 4, "a quarter second at 60 Hz");
    }

    #[test]
    fn rtt_follows_the_link_delay() {
        // A transport with a fixed 50 ms one-way delay, built from the session emulator on both sides.
        let s = EmulSettings {
            latency_ms: 50.0,
            ..Default::default()
        };
        let mut server = Session::server(SessionConfig::new(60));
        let mut client = Session::client(SessionConfig::new(60));
        client.set_emulation(s, EmulSettings::default(), 1);
        server.set_emulation(s, EmulSettings::default(), 2);
        let mut p = Pair::with(server, client, 2);
        for _ in 0..240 {
            p.step();
        }
        let rtt = p.client.rtt_ms(SERVER).unwrap();
        // 100 ms plus up to a tick of release granularity each way plus the reply's tick.
        assert!((100.0..=150.0).contains(&rtt), "{rtt}");
        let diff = p.client.clock().server_tick_estimate() - p.server.current_tick() as f64;
        assert!(diff.abs() <= 1.5, "{diff}");
    }

    #[test]
    fn emulated_loss_on_receive_blocks_the_handshake() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 2);
        p.server.set_emulation(
            EmulSettings::default(),
            EmulSettings {
                loss: 1.0,
                ..Default::default()
            },
            3,
        );
        for _ in 0..60 {
            p.step();
        }
        assert!(!p.server_events.contains(&Event::PeerJoined(2)));
        assert!(!p.client.is_connected());
    }

    #[test]
    fn garbage_is_counted_not_fatal() {
        let mut s = Session::server(SessionConfig::new(60));
        s.peer_connected(5, 0);
        s.receive(5, &[0xff, 1, 2], 0);
        s.receive(5, &[], 0);
        assert_eq!(s.stats().decode_errors, 2);
        assert!(s.poll_event().is_none());
    }

    // ---- WP2: entities -------------------------------------------------------------------------

    use crate::entity::{scene_hash, MANAGER_ROOT};

    const SCENE: &str = "res://tests/fixtures/net/crate.tscn";

    fn root(manager: u64) -> ParentRef {
        ParentRef {
            manager,
            entity: MANAGER_ROOT,
            part: 0,
        }
    }

    fn under(entity: NetId, part: u16) -> ParentRef {
        ParentRef {
            manager: 1,
            entity,
            part,
        }
    }

    fn entity_events(s: &mut Session) -> Vec<EntityEvent> {
        let mut v = Vec::new();
        while let Some(e) = s.poll_entity_event() {
            v.push(e);
        }
        v
    }

    fn created_ids(events: &[EntityEvent]) -> Vec<NetId> {
        events
            .iter()
            .filter_map(|e| match e {
                EntityEvent::Create { create, .. } => Some(create.id),
                _ => None,
            })
            .collect()
    }

    fn connected_pair() -> Pair {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 2);
        for _ in 0..3 {
            p.step();
        }
        assert!(p.client.is_connected());
        p
    }

    #[test]
    fn spawns_apply_in_order_with_scene_paths() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        let a = p
            .server
            .spawn(SCENE, Spawn::new(root(1), "A", 1, 3, std::vec![]), p.now)
            .unwrap();
        let b = p
            .server
            .spawn(
                SCENE,
                Spawn::new(under(a, 2), "B", 1, 3, std::vec![9]),
                p.now,
            )
            .unwrap();
        p.server.reparent(b, root(1), p.now);
        p.server.set_owner(b, 2, p.now);
        p.server.destroy(a, p.now);
        p.step();
        let ev = entity_events(&mut p.client);
        assert_eq!(ev.len(), 5, "{ev:?}");
        match &ev[0] {
            EntityEvent::Create {
                create,
                scene_path,
                from,
                proposal,
            } => {
                assert_eq!((create.id, create.name.as_str()), (a, "A"));
                assert_eq!(scene_path, SCENE);
                assert_eq!((*from, *proposal), (SERVER, false));
                assert_eq!(create.scene_hash, scene_hash(SCENE));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&ev[1], EntityEvent::Create { create, .. } if create.id == b && create.parent == under(a, 2) && create.initial_state == std::vec![9])
        );
        assert!(
            matches!(&ev[2], EntityEvent::Reparent { id, parent, .. } if *id == b && *parent == root(1))
        );
        assert!(matches!(&ev[3], EntityEvent::SetOwner { id, owner: 2, .. } if *id == b));
        assert!(
            matches!(&ev[4], EntityEvent::Destroy { record, cascade: false } if record.id == a)
        );
        assert_eq!(p.client.entities().len(), 1);
        assert_eq!(p.client.entities().get(b).unwrap().owner, 2);
        assert_eq!(p.client.scene_path(scene_hash(SCENE)), Some(SCENE));
        // The scene went over the wire once.
        assert_eq!(
            p.server.stats().decode_errors + p.client.stats().decode_errors,
            0
        );
    }

    #[test]
    fn ids_come_from_the_server_range() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 1);
        let id = p
            .server
            .spawn(SCENE, Spawn::new(root(1), "A", 1, 1, std::vec![]), p.now)
            .unwrap();
        assert_eq!(id, 1, "the server's range starts after the manager root id");
        assert!(p
            .server
            .spawn(
                "res://unknown.tscn",
                Spawn::new(root(1), "X", 1, 1, std::vec![]),
                p.now
            )
            .is_err());
        assert!(p
            .server
            .spawn(
                SCENE,
                Spawn::new(under(99, 0), "X", 1, 1, std::vec![]),
                p.now
            )
            .is_err());
    }

    #[test]
    fn destroying_a_parent_cascades_on_the_client() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 1);
        let a = p
            .server
            .spawn(SCENE, Spawn::new(root(1), "A", 1, 2, std::vec![]), p.now)
            .unwrap();
        let b = p
            .server
            .spawn(
                SCENE,
                Spawn::new(under(a, 1), "B", 1, 2, std::vec![]),
                p.now,
            )
            .unwrap();
        let c = p
            .server
            .spawn(
                SCENE,
                Spawn::new(under(b, 0), "C", 1, 2, std::vec![]),
                p.now,
            )
            .unwrap();
        p.step();
        entity_events(&mut p.client);
        // The server sends one destroy; the client removes the subtree, children first.
        let removed: Vec<NetId> = p.server.destroy(a, p.now).iter().map(|r| r.id).collect();
        assert_eq!(removed, std::vec![c, b, a]);
        p.step();
        let ev = entity_events(&mut p.client);
        let gone: Vec<(NetId, bool)> = ev
            .iter()
            .map(|e| match e {
                EntityEvent::Destroy { record, cascade } => (record.id, *cascade),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(gone, std::vec![(c, true), (b, true), (a, false)]);
        assert!(p.client.entities().is_empty());
    }

    /// Feeds raw entity actions to a connected client, as if they came from the server.
    fn feed(p: &mut Pair, action: EntityAction) {
        let bytes = wire::encode(&Message::Entity(action));
        p.client.receive(SERVER, &bytes, p.now);
    }

    fn create(id: NetId, parent: ParentRef) -> CreateAction {
        CreateAction {
            id,
            scene_hash: scene_hash(SCENE),
            parent,
            name: std::format!("E{id}"),
            owner: 1,
            parts: 2,
            initial_state: std::vec![],
            ..CreateAction::default()
        }
    }

    #[test]
    fn a_create_whose_parent_is_unknown_is_held_until_it_arrives() {
        let mut p = connected_pair();
        feed(
            &mut p,
            EntityAction::Scene {
                hash: scene_hash(SCENE),
                path: SCENE.into(),
                parts_hash: 1,
                props: ScenePropsTable::default(),
            },
        );
        // Child (and its destroy) before its parent, then an unrelated root entity.
        feed(&mut p, EntityAction::Create(create(11, under(10, 1))));
        feed(&mut p, EntityAction::SetOwner { id: 11, owner: 5 });
        feed(&mut p, EntityAction::Create(create(20, root(1))));
        assert_eq!(created_ids(&entity_events(&mut p.client)), std::vec![20]);
        assert_eq!(p.client.held_len(), 2);
        feed(&mut p, EntityAction::Create(create(10, root(1))));
        let ev = entity_events(&mut p.client);
        assert_eq!(created_ids(&ev), std::vec![10, 11]);
        assert!(matches!(
            ev.last(),
            Some(EntityEvent::SetOwner {
                id: 11,
                owner: 5,
                ..
            })
        ));
        assert_eq!(p.client.held_len(), 0);
        assert_eq!(p.client.entities().len(), 3);
    }

    #[test]
    fn a_long_held_create_is_reported_once_and_kept() {
        let mut p = connected_pair();
        feed(&mut p, EntityAction::Create(create(11, under(10, 1))));
        for _ in 0..(5 * 60 + 2) {
            p.step();
        }
        let errors: Vec<&Event> = p
            .client_events
            .iter()
            .filter(|e| matches!(e, Event::EntityError(_)))
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        let Event::EntityError(text) = errors[0] else {
            unreachable!()
        };
        assert!(
            text.contains("create of entity 11") && text.contains("waited"),
            "{text}"
        );
        assert_eq!(p.client.held_len(), 1, "never dropped");
        feed(&mut p, EntityAction::Create(create(10, root(1))));
        assert_eq!(
            created_ids(&entity_events(&mut p.client)),
            std::vec![10, 11]
        );
    }

    #[test]
    fn a_late_joiner_gets_the_table_as_a_burst() {
        let mut server = Session::server(SessionConfig::new(60));
        server.register_scene(SCENE, 77);
        server.tick(0);
        let mut ids = Vec::new();
        let mut parent = root(1);
        for i in 0..50u32 {
            let id = server
                .spawn(
                    SCENE,
                    Spawn::new(parent, &std::format!("N{i}"), 1, 2, std::vec![]),
                    0,
                )
                .unwrap();
            ids.push(id);
            // Every fifth one is a root, the rest nest under the previous one.
            parent = if i % 5 == 4 { root(1) } else { under(id, 1) };
        }
        server.outgoing();
        let mut p = Pair::with(server, Session::client(SessionConfig::new(60)), 3);
        for _ in 0..3 {
            p.step();
        }
        let ev = entity_events(&mut p.client);
        assert_eq!(created_ids(&ev), ids, "all 50, in order");
        assert_eq!(p.client.entities().len(), 50);
        for r in p.server.entities().iter() {
            let c = p.client.entities().get(r.id).unwrap();
            assert_eq!((c.parent, &c.name, c.scene), (r.parent, &r.name, r.scene));
        }
        assert_eq!(p.client.held_len(), 0, "parents always came first");
    }

    #[test]
    fn a_different_part_table_disconnects_naming_the_scene() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 100);
        p.server
            .spawn(SCENE, Spawn::new(root(1), "A", 1, 2, std::vec![]), p.now)
            .unwrap();
        p.step();
        // The client computes its own table when it instantiates the scene.
        p.client.register_scene(SCENE, 101);
        p.step();
        let reason = p
            .client_events
            .iter()
            .find_map(|e| match e {
                Event::Disconnected(r) => Some(r.clone()),
                _ => None,
            })
            .expect("disconnected");
        assert!(
            reason.contains("part table of scene res://tests/fixtures/net/crate.tscn differs"),
            "{reason}"
        );
        // Registered first and announced later: the same check from the other side.
        let mut q = connected_pair();
        q.client.register_scene(SCENE, 5);
        q.server.register_scene(SCENE, 6);
        q.server
            .spawn(SCENE, Spawn::new(root(1), "A", 1, 2, std::vec![]), q.now)
            .unwrap();
        q.step();
        assert!(q.client.is_closed());
    }

    #[test]
    fn owner_allowed_creates_are_proposed_and_relayed() {
        // Two clients on one server, wired by hand.
        let mut server = Session::server(SessionConfig::new(60));
        let mut a = Session::client(SessionConfig::new(60));
        let mut b = Session::client(SessionConfig::new(60));
        let now = 1_000;
        server.peer_connected(2, now);
        server.peer_connected(3, now);
        a.peer_connected(SERVER, now);
        b.peer_connected(SERVER, now);
        let pump = |server: &mut Session, a: &mut Session, b: &mut Session| {
            for _ in 0..3 {
                server.tick(now);
                a.tick(now);
                b.tick(now);
                for o in a.outgoing() {
                    server.receive(2, &o.bytes, now);
                }
                for o in b.outgoing() {
                    server.receive(3, &o.bytes, now);
                }
                for o in server.outgoing() {
                    match o.peer {
                        2 => a.receive(SERVER, &o.bytes, now),
                        3 => b.receive(SERVER, &o.bytes, now),
                        _ => unreachable!(),
                    }
                }
            }
        };
        pump(&mut server, &mut a, &mut b);
        assert!(a.is_connected() && b.is_connected());
        a.register_scene(SCENE, 9);
        let id = a
            .spawn(SCENE, Spawn::new(root(1), "Mine", 2, 2, std::vec![]), now)
            .unwrap();
        assert!(IdAllocator::in_range(a.id_range(), id));
        assert!(!IdAllocator::in_range(b.id_range(), id));
        pump(&mut server, &mut a, &mut b);
        let ev = entity_events(&mut server);
        assert!(
            matches!(&ev[..], [EntityEvent::Create { create, from: 2, proposal: true, scene_path }] if create.id == id && scene_path == SCENE)
        );
        assert!(
            !server.entities().contains(id),
            "not in the table before it is accepted"
        );
        server.register_scene(SCENE, 9);
        assert!(server.accept_create(id, now));
        pump(&mut server, &mut a, &mut b);
        assert_eq!(
            created_ids(&entity_events(&mut b)),
            std::vec![id],
            "relayed to the other client"
        );
        assert!(
            entity_events(&mut a).is_empty(),
            "not echoed to its creator"
        );
        // A refused one: the creator is told to destroy it.
        let id2 = a
            .spawn(SCENE, Spawn::new(root(1), "Mine2", 2, 2, std::vec![]), now)
            .unwrap();
        pump(&mut server, &mut a, &mut b);
        entity_events(&mut server);
        server.refuse_create(id2, now);
        pump(&mut server, &mut a, &mut b);
        assert!(
            matches!(&entity_events(&mut a)[..], [EntityEvent::Destroy { record, .. }] if record.id == id2)
        );
        assert!(entity_events(&mut b).is_empty());
        // Forged: a create in someone else's range, or owned by someone else, is dropped.
        let forged = create(b.id_range().start + 1, root(1));
        let mut bytes = wire::encode(&Message::Entity(EntityAction::Create(CreateAction {
            owner: 2,
            ..forged
        })));
        server.receive(2, &bytes, now);
        bytes = wire::encode(&Message::Entity(EntityAction::SetOwner { id, owner: 2 }));
        server.receive(2, &bytes, now);
        assert_eq!(server.stats().entity_refused, 2);
        // The creator may destroy its own entity; the destroy is relayed.
        assert_eq!(a.destroy(id, now).len(), 1);
        pump(&mut server, &mut a, &mut b);
        assert!(
            matches!(&entity_events(&mut b)[..], [EntityEvent::Destroy { record, .. }] if record.id == id)
        );
        assert!(!server.entities().contains(id));
        // A client may not destroy the server's entities.
        server.register_scene(SCENE, 9);
        let sid = server
            .spawn(SCENE, Spawn::new(root(1), "S", 1, 2, std::vec![]), now)
            .unwrap();
        pump(&mut server, &mut a, &mut b);
        assert!(a.destroy(sid, now).is_empty());
    }

    // ---- WP3: state replication ----------------------------------------------------------------

    use crate::props::{PropDesc, Quant, TypeTag};

    fn props() -> ScenePropsTable {
        ScenePropsTable::new(std::vec![
            std::vec![
                PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("visible", TypeTag::Bool),
            ],
            std::vec![],
            std::vec![PropDesc::new("frame", TypeTag::Int)],
        ])
    }

    fn names() -> Vec<String> {
        std::vec![".".into(), "Sprite".into(), "Marker".into()]
    }

    fn state(x: f32) -> Vec<Vec<Value>> {
        std::vec![
            std::vec![Value::Vector2([x, 1.0]), Value::Bool(true)],
            std::vec![],
            std::vec![Value::Int(3)],
        ]
    }

    /// Steps the pair with the post-tick snapshot pass on both sides.
    fn step_sync(p: &mut Pair) {
        p.step();
        p.server.send_snapshots(p.now);
        p.client.send_snapshots(p.now);
    }

    fn client_registers(p: &mut Pair) {
        p.client.register_scene(SCENE, 42);
        assert!(p.client.register_props(SCENE, props(), names()));
    }

    #[test]
    fn create_carries_state_and_snapshots_follow_changes() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(5.0)),
                p.now,
            )
            .unwrap();
        p.step();
        client_registers(&mut p);
        let ev = entity_events(&mut p.client);
        let EntityEvent::Create { create, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        let init = p
            .client
            .decode_state(create.scene_hash, &create.initial_state)
            .unwrap();
        assert_eq!(
            init,
            std::vec![
                (0, 0, Value::Vector2([5.0, 1.0])),
                (0, 1, Value::Bool(true)),
                (2, 0, Value::Int(3)),
            ]
        );
        // Nothing changed: no snapshot travels.
        for _ in 0..5 {
            step_sync(&mut p);
        }
        assert_eq!(p.server.repl_stats().snapshots_sent, 0);
        assert!(p.client.drain_state_updates().is_empty());
        assert!(p
            .server
            .record_part(id, 0, &[(0, Value::Vector2([6.0, 1.0]))]));
        for _ in 0..3 {
            step_sync(&mut p);
        }
        let ups = p.client.drain_state_updates();
        assert_eq!(ups.len(), 1, "{ups:?}");
        assert_eq!(ups[0].value, Value::Vector2([6.0, 1.0]));
        assert_eq!(p.server.repl_stats().snapshots_sent, 1);
    }

    /// WP5: a replica's `interpolate` field goes to the sample buffer and is written per render frame;
    /// other fields still pass through.
    #[test]
    fn interpolated_fields_of_replicas_are_sampled_not_passed() {
        let mut p = connected_pair();
        let mut table = props();
        table.parts[0][0].interpolate = true;
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, table.clone(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(5.0)),
                p.now,
            )
            .unwrap();
        p.step();
        p.client.register_scene(SCENE, 42);
        assert!(p.client.register_props(SCENE, table, names()));
        let _ = entity_events(&mut p.client);
        assert!(p.client.is_interpolated(id, 0, 0));
        assert!(!p.client.is_interpolated(id, 0, 1));
        assert!(
            !p.server.is_interpolated(id, 0, 0),
            "the server never interpolates"
        );
        p.client.interp_seed(id, 0, 0, Value::Vector2([5.0, 1.0]));
        for k in 0..10 {
            p.server.record_part(
                id,
                0,
                &[
                    (0, Value::Vector2([6.0 + k as f32, 1.0])),
                    (1, Value::Bool(k % 2 == 0)),
                ],
            );
            step_sync(&mut p);
        }
        let ups = p.client.drain_state_updates();
        assert!(!ups.is_empty());
        assert!(
            ups.iter().all(|u| u.field == 1),
            "only the bool passes: {ups:?}"
        );
        let out = p.client.interp_frame(p.now);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].0, (id, 0, 0));
        let Value::Vector2(v) = out[0].1 else {
            panic!()
        };
        assert!(v[0] > 5.0 && v[0] < 16.0, "{v:?}");
        assert!(p.client.interp().timeline.stats.snapshots >= 9);
        assert!(p.server.interp_frame(p.now).is_empty());
    }

    #[test]
    fn a_late_joiners_burst_reproduces_current_state() {
        let mut server = Session::server(SessionConfig::new(60));
        server.register_scene(SCENE, 42);
        server.register_props(SCENE, props(), names());
        server.tick(0);
        let id = server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(0.0)),
                0,
            )
            .unwrap();
        server.record_part(id, 0, &[(0, Value::Vector2([42.0, 1.0]))]);
        server.record_part(id, 2, &[(0, Value::Int(9))]);
        server.outgoing();
        let mut p = Pair::with(server, Session::client(SessionConfig::new(60)), 3);
        for _ in 0..3 {
            step_sync(&mut p);
        }
        client_registers(&mut p);
        let ev = entity_events(&mut p.client);
        let EntityEvent::Create { create, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        let init = p
            .client
            .decode_state(create.scene_hash, &create.initial_state)
            .unwrap();
        assert!(
            init.contains(&(0, 0, Value::Vector2([42.0, 1.0]))),
            "{init:?}"
        );
        assert!(init.contains(&(2, 0, Value::Int(9))));
        assert_eq!(
            p.server.repl_stats().snapshots_sent,
            0,
            "the burst already carried the state"
        );
    }

    #[test]
    fn a_different_property_table_disconnects_naming_scene_and_property() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        p.server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(0.0)),
                p.now,
            )
            .unwrap();
        p.step();
        p.client.register_scene(SCENE, 42);
        let mut theirs = props();
        theirs.parts[2][0].schedule = crate::props::Schedule::Hot;
        assert!(!p.client.register_props(SCENE, theirs, names()));
        p.step();
        let reason = p
            .client_events
            .iter()
            .find_map(|e| match e {
                Event::Disconnected(r) => Some(r.clone()),
                _ => None,
            })
            .expect("disconnected");
        assert!(
            reason.starts_with(&std::format!(
                "protocol mismatch: the property table of scene {SCENE} differs at part Marker, property frame"
            )),
            "{reason}"
        );
    }

    #[test]
    fn profiles_are_part_of_the_hello_hash() {
        let mut a = SessionConfig::new(60);
        a.profiles_hash = 1;
        let mut b = SessionConfig::new(60);
        b.profiles_hash = 2;
        let mut p = Pair::new(a, b, 2);
        for _ in 0..5 {
            p.step();
        }
        assert!(p.client_events.iter().any(|e| matches!(e,
            Event::Disconnected(r) if r.starts_with("protocol mismatch: profiles differs"))));
    }

    #[test]
    fn snapshots_survive_loss_through_the_emulator() {
        let mut p = connected_pair();
        let lossy = crate::emul::EmulSettings {
            loss: 0.3,
            ..crate::emul::EmulSettings::default()
        };
        p.server
            .set_emulation(lossy, crate::emul::EmulSettings::default(), 7);
        p.client
            .set_emulation(lossy, crate::emul::EmulSettings::default(), 9);
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(0.0)),
                p.now,
            )
            .unwrap();
        let mut last = Value::Nil;
        let mut seen = Value::Nil;
        for t in 0..300u32 {
            if t == 2 {
                client_registers(&mut p);
            }
            if t < 200 {
                last = Value::Vector2([t as f32, 1.0]);
                p.server.record_part(id, 0, &[(0, last.clone())]);
            }
            step_sync(&mut p);
            for u in p.client.drain_state_updates() {
                if (u.part, u.field) == (0, 0) {
                    seen = u.value;
                }
            }
        }
        assert_eq!(seen, last, "the latest value arrived despite 30% loss");
    }

    // WP4: input and prediction through the session.

    /// A connected pair with entity `id` of SCENE owned by the client (peer 2), tracked by the client.
    fn predicted_pair(policy: Policy) -> (Pair, NetId) {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "P", 2, 3, std::vec![]).with_values(state(0.0)),
                p.now,
            )
            .unwrap();
        p.step();
        client_registers(&mut p);
        let ev = entity_events(&mut p.client);
        let EntityEvent::Create { create, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        let init = p
            .client
            .decode_state(create.scene_hash, &create.initial_state)
            .unwrap();
        assert!(p.client.track_prediction(id, policy, &init));
        (p, id)
    }

    struct Sim {
        server_x: f32,
        client_x: f32,
        rollbacks: Vec<Rollback>,
    }

    /// One tick of a "move right while pressed" entity on both sides: the client samples `press`,
    /// simulates its predicted tick with the frame for it and records; the server simulates with the
    /// frame it holds for its tick and records. `marker` stamps each frame with its tick.
    fn sim_step(p: &mut Pair, id: NetId, sim: &mut Sim, press: bool) {
        p.step();
        if let Some(r) = p.client.take_rollback() {
            // Restore and replay like gdnet does.
            let Value::Vector2([x, _]) = r.entities[0].1[0].2 else {
                panic!()
            };
            sim.client_x = x;
            let now = p.client.predicted_tick();
            for t in r.to_tick + 1..now {
                if p.client.owner_frame(id, t).is_some_and(|f| f.is_pressed(0)) {
                    sim.client_x += 1.0;
                }
                p.client
                    .record_prediction(id, t, std::vec![Value::Vector2([sim.client_x, 1.0])]);
            }
            sim.rollbacks.push(r);
        }
        let mut f = InputFrame::new(2);
        f.set_action(0, press, 1.0);
        let stamp = p.client.predicted_tick() + u64::from(p.client.input_delay());
        f.set(input::name_hash("tick"), Value::Int(stamp as i64));
        let t = p.client.local_input(id, f);
        assert_eq!(t, stamp);
        let pt = p.client.predicted_tick();
        if p.client
            .owner_frame(id, pt)
            .is_some_and(|f| f.is_pressed(0))
        {
            sim.client_x += 1.0;
        }
        p.client
            .record_prediction(id, pt, std::vec![Value::Vector2([sim.client_x, 1.0])]);
        let st = p.server.current_tick();
        if let Some(taken) = p.server.server_input(id) {
            assert_eq!(taken.tick, st);
            if !taken.repeated && taken.frame.get(input::name_hash("tick")).is_some() {
                assert_eq!(
                    taken.frame.get(input::name_hash("tick")),
                    Some(&Value::Int(st as i64)),
                    "the frame lands on the tick it was stamped with"
                );
            }
            if taken.frame.is_pressed(0) {
                sim.server_x += 1.0;
            }
        }
        p.server
            .record_part(id, 0, &[(0, Value::Vector2([sim.server_x, 1.0]))]);
        p.server.send_snapshots(p.now);
        p.client.send_snapshots(p.now);
    }

    /// The owner's prediction for the server's current tick equals the server's state.
    fn assert_converged(p: &Pair, id: NetId, sim: &Sim) {
        let st = p.server.current_tick();
        let h = p
            .client
            .prediction()
            .history_at(id, st)
            .expect("history at the server tick");
        assert_eq!(
            h[0].2,
            Value::Vector2([sim.server_x, 1.0]),
            "the prediction for tick {st} (lead {:?}, client now {})",
            p.client.prediction_lead(),
            sim.client_x
        );
    }

    #[test]
    fn predicted_input_lands_on_the_server_tick_and_converges() {
        let (mut p, id) = predicted_pair(Policy::Check);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        for _ in 0..30 {
            sim_step(&mut p, id, &mut sim, false);
        }
        assert!(p.client.prediction_lead().is_some_and(|l| l >= 2));
        for _ in 0..20 {
            sim_step(&mut p, id, &mut sim, true);
        }
        for _ in 0..20 {
            sim_step(&mut p, id, &mut sim, false);
        }
        assert_eq!(sim.client_x, 20.0, "the owner moved immediately, 20 ticks");
        assert_eq!(sim.server_x, 20.0, "the server ran the same 20 frames");
        assert!(sim.rollbacks.is_empty(), "{:?}", sim.rollbacks);
        assert!(p.client.predict_stats().confirmations > 0);
        assert_eq!(p.server.input_stats().refused, 0);
        // Hot fields of a predicted entity never reach gdnet as plain updates.
        assert!(p
            .client
            .drain_state_updates()
            .iter()
            .all(|u| u.part != 0 || u.field != 0));
    }

    #[test]
    fn a_server_impulse_rolls_back_once_to_its_tick() {
        let (mut p, id) = predicted_pair(Policy::Check);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        for _ in 0..30 {
            sim_step(&mut p, id, &mut sim, true);
        }
        assert!(sim.rollbacks.is_empty());
        sim.server_x += 50.0;
        let impulse_tick = p.server.current_tick() + 1;
        for _ in 0..30 {
            sim_step(&mut p, id, &mut sim, true);
        }
        assert_eq!(sim.rollbacks.len(), 1, "{:?}", sim.rollbacks);
        assert_eq!(sim.rollbacks[0].to_tick, impulse_tick);
        assert_converged(&p, id, &sim);
    }

    #[test]
    fn disabled_entities_get_their_state_and_never_roll_back() {
        let (mut p, id) = predicted_pair(Policy::Disabled);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        let mut seen = 0;
        for i in 0..40 {
            if i == 20 {
                sim.server_x += 50.0;
            }
            sim_step(&mut p, id, &mut sim, true);
            seen += p
                .client
                .drain_state_updates()
                .iter()
                .filter(|u| u.id == id && u.part == 0 && u.field == 0)
                .count();
        }
        assert!(sim.rollbacks.is_empty());
        assert!(seen > 0, "snapshot values pass through");
    }

    #[test]
    fn always_rolls_back_at_every_confirmation() {
        let (mut p, id) = predicted_pair(Policy::Always);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        for _ in 0..30 {
            sim_step(&mut p, id, &mut sim, true);
        }
        assert!(sim.rollbacks.len() > 10, "{}", sim.rollbacks.len());
        assert_converged(&p, id, &sim);
    }

    #[test]
    fn delay_shifts_the_applied_tick() {
        let (mut p, id) = predicted_pair(Policy::Check);
        p.client.set_input_delay(2);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        for _ in 0..30 {
            sim_step(&mut p, id, &mut sim, false);
        }
        sim_step(&mut p, id, &mut sim, true);
        assert_eq!(sim.client_x, 0.0, "a press is not applied on its tick");
        sim_step(&mut p, id, &mut sim, true);
        assert_eq!(sim.client_x, 0.0);
        sim_step(&mut p, id, &mut sim, true);
        assert_eq!(sim.client_x, 1.0, "two ticks later");
        for _ in 0..20 {
            sim_step(&mut p, id, &mut sim, false);
        }
        assert_eq!(
            sim.client_x, sim.server_x,
            "both stopped after the same presses"
        );
        assert!(sim.rollbacks.is_empty(), "{:?}", sim.rollbacks);
    }

    #[test]
    fn input_from_a_non_owner_is_refused() {
        let (mut p, id) = predicted_pair(Policy::Check);
        p.server.set_owner(id, SERVER, p.now);
        let mut sim = Sim {
            server_x: 0.0,
            client_x: 0.0,
            rollbacks: Vec::new(),
        };
        for _ in 0..5 {
            sim_step(&mut p, id, &mut sim, true);
        }
        assert!(p.server.input_stats().refused > 0);
        assert!(
            p.server.server_input(id).is_none(),
            "a server-owned entity takes no frames"
        );
        assert_eq!(sim.server_x, 0.0);
    }

    #[test]
    fn a_lost_frame_is_repeated_by_the_server() {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "P", 2, 3, std::vec![]).with_values(state(0.0)),
                p.now,
            )
            .unwrap();
        // No input ever arrives: every tick repeats (an empty frame).
        for _ in 0..4 {
            p.step();
        }
        let t = p.server.server_input(id).unwrap();
        assert!(t.repeated);
        assert!(p.server.input_stats().repeated >= 4);
    }

    #[test]
    fn adaptive_delay_ramps_only_while_interacting() {
        let mut c = Session::client(SessionConfig::new(60));
        c.set_adaptive_delay(true);
        assert_eq!(c.input_delay(), 0);
        c.set_input_delay(1);
        assert_eq!(c.input_delay(), 1);
        assert!(c.adaptive_delay().is_some_and(|a| !a.is_interacting()));
        c.set_interaction(true);
        c.set_adaptive_delay(false);
        assert!(c.adaptive_delay().is_none());
    }

    // ---- WP7: authority and keyed state --------------------------------------------------------

    use crate::authority::Transfer;
    use crate::keyed::{KeyEvent, KeyMsg, Who};

    /// A server and two clients (transport ids 2 and 3) with an instant transport; every step runs the
    /// tick and the post-tick send on all three.
    struct Trio {
        server: Session,
        a: Session,
        b: Session,
        now: u64,
    }

    impl Trio {
        fn new() -> Self {
            let mut t = Self {
                server: Session::server(SessionConfig::new(60)),
                a: Session::client(SessionConfig::new(60)),
                b: Session::client(SessionConfig::new(60)),
                now: 1_000_000,
            };
            t.server.peer_connected(2, t.now);
            t.server.peer_connected(3, t.now);
            t.a.peer_connected(SERVER, t.now);
            t.b.peer_connected(SERVER, t.now);
            for _ in 0..3 {
                t.step();
            }
            assert!(t.a.is_connected() && t.b.is_connected());
            for s in [&mut t.server, &mut t.a, &mut t.b] {
                s.register_scene(SCENE, 42);
                s.register_props(SCENE, props(), names());
            }
            t
        }

        fn step(&mut self) {
            self.now += TICK_US;
            for s in [&mut self.server, &mut self.a, &mut self.b] {
                s.tick(self.now);
                s.send_snapshots(self.now);
            }
            self.pump();
        }

        fn pump(&mut self) {
            let now = self.now;
            for o in self.a.outgoing() {
                self.server.receive(2, &o.bytes, now);
            }
            for o in self.b.outgoing() {
                self.server.receive(3, &o.bytes, now);
            }
            for o in self.server.outgoing() {
                match o.peer {
                    2 => self.a.receive(SERVER, &o.bytes, now),
                    3 => self.b.receive(SERVER, &o.bytes, now),
                    _ => unreachable!(),
                }
            }
        }

        fn steps(&mut self, n: usize) {
            for _ in 0..n {
                self.step();
            }
        }

        fn spawn(&mut self, owner: PeerId, transfer: Transfer, client_owned: bool) -> NetId {
            let id = self
                .server
                .spawn(
                    SCENE,
                    Spawn::new(root(1), "E", owner, 3, std::vec![])
                        .with_values(state(1.0))
                        .with_authority(transfer, client_owned),
                    self.now,
                )
                .unwrap();
            self.steps(2);
            for s in [&mut self.server, &mut self.a, &mut self.b] {
                entity_events(s);
            }
            id
        }

        fn owner(&self, id: NetId) -> [PeerId; 3] {
            [&self.server, &self.a, &self.b].map(|s| s.entities().get(id).map_or(0, |r| r.owner))
        }
    }

    fn replies(events: &[EntityEvent]) -> Vec<(NetId, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                EntityEvent::OwnerReply { id, granted, .. } => Some((*id, *granted)),
                _ => None,
            })
            .collect()
    }

    fn owner_sets(events: &[EntityEvent]) -> Vec<(NetId, PeerId)> {
        events
            .iter()
            .filter_map(|e| match e {
                EntityEvent::SetOwner { id, owner, .. } => Some((*id, *owner)),
                _ => None,
            })
            .collect()
    }

    fn queries(events: &[EntityEvent]) -> Vec<(NetId, PeerId)> {
        events
            .iter()
            .filter_map(|e| match e {
                EntityEvent::OwnerQuery { id, requester, .. } => Some((*id, *requester)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn take_request_fixed_and_release() {
        let mut t = Trio::new();
        let fixed = t.spawn(SERVER, Transfer::Fixed, false);
        let take = t.spawn(SERVER, Transfer::Take, false);
        let req = t.spawn(SERVER, Transfer::Request, false);
        assert_eq!(
            t.a.entities().get(take).map(|r| r.transfer),
            Some(Transfer::Take),
            "the rule travels in the create"
        );
        // take: always granted, one SetOwner on every peer, then the reply to the requester.
        t.a.request_owner(take, t.now).unwrap();
        t.steps(2);
        assert_eq!(t.owner(take), [2, 2, 2]);
        assert_eq!(owner_sets(&entity_events(&mut t.server)), [(take, 2)]);
        let ev = entity_events(&mut t.a);
        assert_eq!(owner_sets(&ev), [(take, 2)]);
        assert_eq!(replies(&ev), [(take, true)]);
        assert_eq!(owner_sets(&entity_events(&mut t.b)), [(take, 2)]);
        // fixed: refused.
        t.a.request_owner(fixed, t.now).unwrap();
        t.steps(2);
        assert_eq!(t.owner(fixed), [1, 1, 1]);
        assert_eq!(replies(&entity_events(&mut t.a)), [(fixed, false)]);
        assert!(owner_sets(&entity_events(&mut t.b)).is_empty());
        // request of a server-owned entity: the server's gdnet decides.
        t.a.request_owner(req, t.now).unwrap();
        t.steps(2);
        assert_eq!(queries(&entity_events(&mut t.server)), [(req, 2)]);
        t.server.answer_owner_query(req, 2, true, t.now);
        t.steps(2);
        assert_eq!(t.owner(req), [2, 2, 2]);
        assert_eq!(replies(&entity_events(&mut t.a)), [(req, true)]);
        // request of a client-owned entity: the owner is asked; a refusal keeps it.
        t.b.request_owner(req, t.now).unwrap();
        t.steps(2);
        assert_eq!(queries(&entity_events(&mut t.a)), [(req, 3)]);
        t.a.answer_owner_query(req, 3, false, t.now);
        t.steps(2);
        assert_eq!(t.owner(req), [2, 2, 2]);
        assert_eq!(replies(&entity_events(&mut t.b)), [(req, false)]);
        t.b.request_owner(req, t.now).unwrap();
        t.steps(2);
        entity_events(&mut t.a);
        t.a.answer_owner_query(req, 3, true, t.now);
        t.steps(2);
        assert_eq!(t.owner(req), [3, 3, 3]);
        assert_eq!(replies(&entity_events(&mut t.b)), [(req, true)]);
        // release: back to the server; only the owner may release.
        assert!(!t.a.release_owner(req, t.now), "a does not own it any more");
        assert!(t.a.release_owner(take, t.now));
        t.steps(2);
        assert_eq!(t.owner(take), [1, 1, 1]);
        // A forged release of someone else's entity changes nothing.
        let bytes = wire::encode(&Message::Entity(EntityAction::ReleaseOwner { id: req }));
        t.server.receive(2, &bytes, t.now);
        t.steps(1);
        assert_eq!(t.owner(req), [3, 3, 3]);
        assert!(t.server.authority_stats().owner_changes >= 4);
        assert_eq!(t.server.authority_stats().requests_refused, 2);
    }

    #[test]
    fn an_unanswered_query_is_refused_after_five_seconds() {
        let mut t = Trio::new();
        let req = t.spawn(2, Transfer::Request, false);
        t.b.request_owner(req, t.now).unwrap();
        t.steps(2);
        assert_eq!(queries(&entity_events(&mut t.a)), [(req, 3)]);
        t.steps(5 * 60);
        assert_eq!(replies(&entity_events(&mut t.b)), [(req, false)]);
        assert_eq!(t.owner(req), [2, 2, 2]);
    }

    #[test]
    fn a_new_owner_starts_a_new_input_stream_and_the_old_ones_input_is_dropped() {
        let mut t = Trio::new();
        let id = t.spawn(2, Transfer::Take, false);
        t.b.request_owner(id, t.now).unwrap();
        t.steps(2);
        assert_eq!(t.owner(id), [3, 3, 3]);
        let before = t.server.input_stats().refused;
        t.a.local_input(id, InputFrame::default());
        t.steps(1);
        assert!(
            t.server.input_stats().refused > before,
            "a's frames are refused now"
        );
    }

    #[test]
    fn nested_entities_inherit_their_parents_owner_unless_they_set_their_own() {
        let mut t = Trio::new();
        let parent = t.spawn(2, Transfer::Fixed, false);
        let inherit = t
            .server
            .spawn(
                SCENE,
                Spawn::new(under(parent, 2), "In", SERVER, 3, std::vec![]).inheriting_owner(),
                t.now,
            )
            .unwrap();
        let own = t
            .server
            .spawn(
                SCENE,
                Spawn::new(under(parent, 2), "Own", SERVER, 3, std::vec![]),
                t.now,
            )
            .unwrap();
        t.steps(2);
        assert_eq!(t.owner(inherit), [2, 2, 2]);
        assert_eq!(t.owner(own), [1, 1, 1]);
    }

    #[test]
    fn a_client_owned_entitys_state_is_accepted_relayed_and_not_echoed() {
        let mut t = Trio::new();
        let id = t.spawn(2, Transfer::Fixed, true);
        t.server.drain_state_updates();
        t.b.drain_state_updates();
        for k in 0..10 {
            t.a.record_part(id, 0, &[(0, Value::Vector2([2.0 + k as f32, 1.0]))]);
            t.step();
        }
        t.steps(2);
        let on_server = t.server.drain_state_updates();
        assert!(
            on_server
                .iter()
                .any(|u| u.id == id && u.value == Value::Vector2([11.0, 1.0])),
            "the server applies the owner's state: {on_server:?}"
        );
        let on_b = t.b.drain_state_updates();
        assert_eq!(
            on_b.last().map(|u| u.value.clone()),
            Some(Value::Vector2([11.0, 1.0])),
            "relayed to the other client"
        );
        assert!(
            t.a.drain_state_updates().iter().all(|u| u.id != id),
            "never echoed to its owner"
        );
        assert!(t.server.authority_stats().owner_states_received > 0);
        // A late joiner's burst carries the owner's latest state.
        let mut c = Session::client(SessionConfig::new(60));
        t.server.peer_connected(4, t.now);
        c.peer_connected(SERVER, t.now);
        c.register_scene(SCENE, 42);
        c.register_props(SCENE, props(), names());
        for o in c.outgoing() {
            t.server.receive(4, &o.bytes, t.now);
        }
        for o in t.server.outgoing() {
            match o.peer {
                4 => c.receive(SERVER, &o.bytes, t.now),
                2 => t.a.receive(SERVER, &o.bytes, t.now),
                _ => t.b.receive(SERVER, &o.bytes, t.now),
            }
        }
        let ev = entity_events(&mut c);
        let init = ev
            .iter()
            .find_map(|e| match e {
                EntityEvent::Create { create, .. } if create.id == id => {
                    c.decode_state(create.scene_hash, &create.initial_state)
                }
                _ => None,
            })
            .unwrap();
        assert!(init.contains(&(0, 0, Value::Vector2([11.0, 1.0]))));
        t.server.peer_disconnected(4, t.now);
        // Once the server owns it again, its own records go to the old owner.
        t.server.set_owner(id, SERVER, t.now);
        t.steps(2);
        t.server
            .record_part(id, 0, &[(0, Value::Vector2([50.0, 1.0]))]);
        t.steps(3);
        assert!(t
            .a
            .drain_state_updates()
            .iter()
            .any(|u| u.id == id && u.value == Value::Vector2([50.0, 1.0])));
    }

    #[test]
    fn the_owner_never_interpolates_its_client_owned_entity() {
        let mut t = Trio::new();
        let mut table = props();
        table.parts[0][0].interpolate = true;
        for s in [&mut t.server, &mut t.a, &mut t.b] {
            s.register_props(SCENE, table.clone(), names());
        }
        let id = t.spawn(2, Transfer::Fixed, true);
        assert!(!t.a.is_interpolated(id, 0, 0), "the owner simulates it");
        assert!(
            t.b.is_interpolated(id, 0, 0),
            "another client interpolates it"
        );
    }

    #[test]
    fn a_write_to_an_entity_the_client_does_not_own_is_rejected() {
        let mut t = Trio::new();
        let id = t.spawn(SERVER, Transfer::Fixed, false);
        t.a.remember_received(
            id,
            &[
                (0, 0, Value::Vector2([1.0, 1.0])),
                (0, 1, Value::Bool(true)),
            ],
        );
        // No change, no write; an interpolated or engine-driven field never counts.
        assert_eq!(t.a.check_writes(id, 0, &[(1, Value::Bool(true))]), 0);
        assert_eq!(t.a.check_writes(id, 2, &[(0, Value::Int(9))]), 0, "frame");
        assert_eq!(t.a.check_writes(id, 0, &[(1, Value::Bool(false))]), 1);
        assert_eq!(
            t.a.check_writes(id, 0, &[(1, Value::Bool(false))]),
            0,
            "sent once until answered"
        );
        t.steps(2);
        let ev = entity_events(&mut t.server);
        assert!(
            matches!(&ev[..], [EntityEvent::WriteRejected { id: i, peer: 2, .. }] if *i == id),
            "{ev:?}"
        );
        let ev = entity_events(&mut t.a);
        let restore = ev
            .iter()
            .find_map(|e| match e {
                EntityEvent::WriteRejected {
                    restore, peer: 2, ..
                } => Some(restore.clone()),
                _ => None,
            })
            .expect("the writer is told");
        assert_eq!(restore, std::vec![(0, 1, Value::Bool(true))]);
        assert_eq!(t.server.authority_stats().writes_rejected, 1);
        assert_eq!(t.a.authority_stats().writes_rejected, 1);
        // A write that keeps coming back (code rewriting it every poll) stops being checked after
        // three rejects in a row.
        for _ in 0..5 {
            t.a.check_writes(id, 0, &[(1, Value::Bool(false))]);
            t.steps(2);
        }
        assert_eq!(
            t.a.authority_stats().writes_rejected,
            MAX_REJECTS_IN_A_ROW as u64
        );
        entity_events(&mut t.server);
        entity_events(&mut t.a);
        // The server's value never changed and nothing reached b.
        assert!(t.b.drain_state_updates().iter().all(|u| u.id != id));
        // An owner state for an entity that is not client_owned is a rejected write too.
        let own = t.spawn(2, Transfer::Fixed, false);
        t.a.record_part(own, 0, &[(1, Value::Bool(false))]);
        let bytes = wire::encode(&Message::OwnerState(crate::authority::StateMsg {
            tick: 1,
            entities: std::vec![(own, t.a.replication().encode_full(own))],
        }));
        t.server.receive(2, &bytes, t.now);
        t.steps(1);
        assert!(entity_events(&mut t.server)
            .iter()
            .any(|e| matches!(e, EntityEvent::WriteRejected { id: i, .. } if *i == own)));
        assert!(t.server.drain_state_updates().is_empty(), "not applied");
    }

    fn key_events(s: &mut Session) -> Vec<KeyEvent> {
        core::iter::from_fn(|| s.poll_key_event()).collect()
    }

    #[test]
    fn keyed_state_deltas_erase_and_the_full_map_on_join() {
        let mut p = Pair::new(SessionConfig::new(60), SessionConfig::new(60), 2);
        p.server.keyed_set("score/1", std::vec![10], p.now).unwrap();
        p.server.keyed_set("title", std::vec![1, 2], p.now).unwrap();
        for _ in 0..3 {
            p.step();
        }
        assert_eq!(p.client.keyed().get("score/1"), Some(&std::vec![10]));
        assert_eq!(p.client.keyed().keys(""), std::vec!["score/1", "title"]);
        assert_eq!(
            key_events(&mut p.client).len(),
            2,
            "the late joiner's full map"
        );
        p.server.keyed_set("score/1", std::vec![11], p.now).unwrap();
        p.server.keyed_set("score/1", std::vec![11], p.now).unwrap();
        p.server.keyed_erase("title", p.now).unwrap();
        p.step();
        assert_eq!(
            key_events(&mut p.client),
            std::vec![
                KeyEvent::Changed("score/1".into(), std::vec![11]),
                KeyEvent::Erased("title".into()),
            ],
            "deltas only on change"
        );
        assert!(p.server.keyed().stats.bytes_sent > 0);
        assert!(p.client.keyed().stats.bytes_received > 0);
    }

    #[test]
    fn keyed_writes_need_a_permission() {
        let mut t = Trio::new();
        let err = t.a.keyed_set("chat/1", std::vec![1], t.now).unwrap_err();
        assert!(err.contains("chat/1"), "{err}");
        t.server.keyed_allow("chat/", Who::All, t.now);
        t.server.keyed_allow("score/", Who::Owner, t.now);
        t.steps(1);
        t.a.keyed_set("chat/1", std::vec![7], t.now).unwrap();
        assert_eq!(
            t.a.keyed().get("chat/1"),
            Some(&std::vec![7]),
            "applied at once"
        );
        t.a.keyed_set("score/2", std::vec![3], t.now).unwrap();
        assert!(t.a.keyed_set("score/3", std::vec![3], t.now).is_err());
        t.steps(2);
        for s in [&t.server, &t.b] {
            assert_eq!(s.keyed().get("chat/1"), Some(&std::vec![7]));
            assert_eq!(s.keyed().get("score/2"), Some(&std::vec![3]));
        }
        // A write the server refuses (sent past the local check) is rolled back on the writer.
        key_events(&mut t.a);
        t.server.keyed_set("admin", std::vec![1], t.now).unwrap();
        t.steps(1);
        let forged = wire::encode(&Message::Keyed(KeyMsg::Set {
            path: "admin".into(),
            value: std::vec![9],
        }));
        t.server.receive(2, &forged, t.now);
        t.steps(2);
        assert_eq!(t.server.keyed().get("admin"), Some(&std::vec![1]));
        assert_eq!(t.b.keyed().get("admin"), Some(&std::vec![1]));
        assert!(key_events(&mut t.a).contains(&KeyEvent::Rejected("admin".into())));
        assert_eq!(t.server.keyed().stats.rejected, 1);
    }

    #[test]
    fn a_view_hides_a_path_from_the_filtered_peer() {
        let mut t = Trio::new();
        t.server
            .keyed_set("team/red/plan", std::vec![1], t.now)
            .unwrap();
        t.server.keyed_set("open", std::vec![2], t.now).unwrap();
        t.steps(1);
        assert!(t.b.keyed().get("team/red/plan").is_some());
        key_events(&mut t.b);
        t.server
            .keyed_view("team/red/", &[(2, true), (3, false)], t.now);
        t.steps(1);
        assert_eq!(
            key_events(&mut t.b),
            std::vec![KeyEvent::Erased("team/red/plan".into())]
        );
        assert!(
            t.a.keyed().get("team/red/plan").is_some(),
            "a still sees it"
        );
        t.server
            .keyed_set("team/red/plan", std::vec![5], t.now)
            .unwrap();
        t.steps(1);
        assert_eq!(t.a.keyed().get("team/red/plan"), Some(&std::vec![5]));
        assert!(t.b.keyed().get("team/red/plan").is_none());
        assert!(t.b.keyed().get("open").is_some());
        t.server.keyed_set_visible("team/red/", 3, true, t.now);
        t.steps(1);
        assert_eq!(t.b.keyed().get("team/red/plan"), Some(&std::vec![5]));
    }

    // WP8: hash telemetry.

    /// A connected pair with one entity (hot `position` in mm) known on both sides, the client holding
    /// its `Create` state as gdnet would.
    fn hashed_pair() -> (Pair, NetId) {
        let mut p = connected_pair();
        p.server.register_scene(SCENE, 42);
        p.server.register_props(SCENE, props(), names());
        let id = p
            .server
            .spawn(
                SCENE,
                Spawn::new(root(1), "A", 1, 3, std::vec![]).with_values(state(5.0)),
                p.now,
            )
            .unwrap();
        p.step();
        client_registers(&mut p);
        let ev = entity_events(&mut p.client);
        let EntityEvent::Create { create, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        let init = p
            .client
            .decode_state(create.scene_hash, &create.initial_state)
            .unwrap();
        p.client.remember_received(id, &init);
        (p, id)
    }

    #[test]
    fn a_one_millimetre_change_changes_the_entity_hash() {
        let (mut p, id) = hashed_pair();
        let before = p.server.replication().entity_hash(id).unwrap();
        assert!(p
            .server
            .record_part(id, 0, &[(0, Value::Vector2([5.001, 1.0]))]));
        let after = p.server.replication().entity_hash(id).unwrap();
        assert_ne!(before, after);
        // Back to the same value: the same hash.
        p.server
            .record_part(id, 0, &[(0, Value::Vector2([5.0, 1.0]))]);
        assert_eq!(p.server.replication().entity_hash(id), Some(before));
        // Cold fields do not count.
        p.server.record_part(id, 0, &[(1, Value::Bool(false))]);
        assert_eq!(p.server.replication().entity_hash(id), Some(before));
    }

    #[test]
    fn hashes_match_while_moving_and_count_a_held_state_edit() {
        let (mut p, id) = hashed_pair();
        for i in 0..30 {
            p.server
                .record_part(id, 0, &[(0, Value::Vector2([5.0 + i as f32 * 0.1, 1.0]))]);
            step_sync(&mut p);
            let _ = p.client.drain_state_updates();
        }
        for _ in 0..10 {
            step_sync(&mut p);
        }
        let s = p.client.hash_stats();
        assert!(
            p.server.hash_stats().sent >= 30,
            "{:?}",
            p.server.hash_stats()
        );
        assert!(s.received >= 30, "{s:?}");
        assert!(s.compared >= 30, "{s:?}");
        assert_eq!(s.divergences, 0, "{s:?}");
        assert_eq!(p.client.divergences_of(id), 0);
        let tick = p.server.current_tick();
        let client_state = p.client.state_hash(tick);
        assert_eq!(client_state, p.server.state_hash(tick));
        assert_eq!(
            p.server.state_hash(tick - 1),
            None,
            "the server only knows the current tick"
        );
        // Idle: the unchanged hash is not resent within the refresh period (gate G4).
        let sent = p.server.hash_stats().sent;
        for _ in 0..30 {
            step_sync(&mut p);
        }
        assert_eq!(p.server.hash_stats().sent, sent);
        // The client's held copy goes wrong: counted at the next comparisons, never fatal.
        p.server.set_hash_refresh(0.0);
        p.client
            .debug_corrupt_held(id, 0, 0, &Value::Vector2([99.0, 1.0]));
        for _ in 0..3 {
            step_sync(&mut p);
        }
        assert!(p.client.hash_stats().divergences >= 1);
        assert!(p.client.divergences_of(id) >= 1);
        assert!(p.client.is_connected());
    }

    #[test]
    fn lost_snapshots_are_skipped_not_counted() {
        let (mut p, id) = hashed_pair();
        // Every second packet to the client is dropped by hand: snapshots and hashes alike.
        for i in 0..40 {
            p.server
                .record_part(id, 0, &[(0, Value::Vector2([5.0 + i as f32 * 0.1, 1.0]))]);
            p.step();
            p.server.send_snapshots(p.now);
            let out = p.server.outgoing();
            for (k, o) in out.into_iter().enumerate() {
                if (k + i) % 2 == 0 {
                    p.client.receive(SERVER, &o.bytes, p.now);
                }
            }
            p.client.send_snapshots(p.now);
            for o in p.client.outgoing() {
                p.server.receive(p.id, &o.bytes, p.now);
            }
            let _ = p.client.drain_state_updates();
        }
        let s = p.client.hash_stats();
        assert_eq!(s.divergences, 0, "{s:?}");
        assert!(s.compared > 0, "{s:?}");
    }

    #[test]
    fn hash_telemetry_off_sends_nothing() {
        let (mut p, id) = hashed_pair();
        p.server.set_hash_telemetry(false, 16);
        for i in 0..10 {
            p.server
                .record_part(id, 0, &[(0, Value::Vector2([6.0 + i as f32, 1.0]))]);
            step_sync(&mut p);
        }
        assert_eq!(p.server.hash_stats().sent, 0);
        assert_eq!(p.client.hash_stats().received, 0);
    }
}
