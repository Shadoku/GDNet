//! `NetSession`: one gdnet session bound to one `MultiplayerAPI` (docs/PLAN_NETCODE.md 2.2, 2.3, WP1).
//!
//! Design: a session is a `Node`. Its `_physics_process` (process priority -1000, so it runs before the
//! game's nodes) is the per-tick loop of 2.2: step 1 drains the packets `SceneMultiplayer.peer_packet`
//! delivered since the last tick into netcore, netcore ticks, and step 6 hands netcore's outgoing
//! packets to `SceneMultiplayer.send_bytes()`. Then the `tick` signal fires. The node must be inside
//! the tree for this to run.
//!
//! `start(peer, root_path)` wraps a `SceneMultiplayer` in a `NetMultiplayerApi` and installs it with
//! `SceneTree.set_multiplayer(api, root_path)`: with an empty path it wraps the tree's default
//! `SceneMultiplayer` (this is what `Net` does); with a path it uses a fresh `SceneMultiplayer` for that
//! branch, which is how one headless process holds a server branch and client branches
//! (`tests/framework/net_fixture.gd`). `leave()` closes the peer and puts back what was there.
//!
//! The `Net` singleton owns one default session; tests and tools create more directly
//! (`NetSession.new()`), one per branch.

use crate::api_ext::NetMultiplayerApi;
use crate::call::{self as net_call, NetArg, NetCall};
use crate::emulation::{settings_of, NetEmulation};
use crate::interp;
use crate::keyed::{self as net_keyed, NetKeyedState};
use crate::manager::{entity_root_of, NetManager, TickCtx, MANAGERS_GROUP};
use crate::pipe_peer::NetPipePeer;
use crate::poll::{NetPoller, POLLER_NODE};
use crate::profiles;
use crate::replay;
use crate::tick;
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::node::InternalMode;
use godot::classes::{
    Engine, INode, MultiplayerApi, MultiplayerPeer, Node, OfflineMultiplayerPeer, SceneMultiplayer,
    SceneTree, Time,
};
use godot::global::Error;
use godot::prelude::*;
use netcore::call::Verb;
use netcore::session::EntityEvent;
use netcore::{EmulSettings, Event, KeyEvent, Reliability, Session, SessionConfig, StateUpdate};
use netcore::{NetId, ParentRef};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

/// Meta on a session's `NetMultiplayerApi`: the session's instance id (how a `NetManager` finds the
/// session of its branch through `Node.get_multiplayer()`).
pub const META_SESSION: &str = "_net_session";

/// Packets every gdnet session of this process handed to a transport (gate G0's probe).
static PACKETS_SENT: AtomicU64 = AtomicU64::new(0);
/// Session generations: each `start()` takes a new one, so managers notice a new session.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// What to put back on `leave()`.
enum Restore {
    /// The default multiplayer of the tree, and the peer its `SceneMultiplayer` had.
    Default {
        api: Gd<MultiplayerApi>,
        peer: Option<Gd<MultiplayerPeer>>,
    },
    /// A branch: the custom multiplayer that was configured for the path, if any.
    Branch {
        path: NodePath,
        previous: Option<Gd<MultiplayerApi>>,
    },
}

/// The callbacks a session connects on the wrapped `SceneMultiplayer`.
/// WP5: the tree signal the owned offsets come off at (emitted at the start of every physics frame,
/// before any node's `_physics_process`, and before code awaiting it resumes).
const PHYSICS_FRAME_HOOK: (&str, &str) = ("physics_frame", "_on_physics_frame");

const HOOKS: [(&str, &str); 6] = [
    ("peer_packet", "_on_peer_packet"),
    ("peer_connected", "_on_peer_connected"),
    ("peer_disconnected", "_on_peer_disconnected"),
    ("connected_to_server", "_on_connected_to_server"),
    ("connection_failed", "_on_connection_failed"),
    ("server_disconnected", "_on_server_disconnected"),
];

#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct NetSession {
    base: Base<Node>,
    pub(crate) core: Option<Session>,
    api: Option<Gd<NetMultiplayerApi>>,
    inner: Option<Gd<SceneMultiplayer>>,
    peer: Option<Gd<MultiplayerPeer>>,
    restore: Option<Restore>,
    inbox: Vec<(i32, Vec<u8>)>,
    emulation: Option<Gd<NetEmulation>>,
    applied: Option<(EmulSettings, EmulSettings)>,
    /// Test hook (docs/PLAN_NETCODE.md 3.3): any value but 0 changes this side's protocol hash, so the
    /// handshake fails with a reason naming the salt. Set it before `start()`.
    #[var(rename = _debug_protocol_salt, get = get_debug_protocol_salt, set = set_debug_protocol_salt)]
    pub(crate) debug_protocol_salt: i64,
    /// Test hook (WP2): added to every part table hash this side registers, so a spawn fails the scene
    /// check with a reason naming the scene. Set it before the first spawn.
    #[var(rename = _debug_part_table_salt, get = get_debug_part_table_salt, set = set_debug_part_table_salt)]
    pub(crate) debug_part_table_salt: i64,
    generation: u64,
    branch_root: Option<Gd<Node>>,
    /// Entity events for a manager not found yet, with the tick they arrived and whether reported.
    unrouted: Vec<(u64, bool, EntityEvent)>,
    /// WP3: snapshot bytes per second per peer; 0 = `Net.budget_bytes_per_s` / the project setting
    /// (re-read every tick). Changes apply at once.
    #[var(get = get_budget_bytes_per_s, set = set_budget_bytes_per_s)]
    budget_bytes_per_s: i64,
    /// WP3: cold properties are polled every this many ticks; 0 = `Net.cold_poll_ticks` / the project
    /// setting (default 6).
    #[var(get = get_cold_poll_ticks, set = set_cold_poll_ticks)]
    cold_poll_ticks: i64,
    /// Test hook (WP3): this side's property tables change the quantiser of every property with this
    /// name, so a spawn fails the property check with a reason naming the scene and property. Set it
    /// before the first spawn.
    #[var(rename = _debug_perturb_property, get = get_debug_perturb_property, set = set_debug_perturb_property)]
    debug_perturb_property: GString,
    poller_added: bool,
    stats: Counters,
    /// WP4: the predicted tick of the previous step (a jump means the lead grew: catch up).
    last_ptick: Option<u64>,
    /// WP5: the effective jitter buffer in ticks (read only; a client's adapts, see interp.rs).
    #[var(get = get_jitter_buffer_ticks, no_set)]
    #[allow(dead_code)]
    jitter_buffer_ticks: i64,
    /// WP5: owned offsets were shown in the last render frame (restore them before the next tick).
    owned_active: bool,
    /// WP6: calls waiting for their `CallReturn`, by request id.
    calls: HashMap<u32, Gd<NetCall>>,
    /// Test hook (WP6): `"<part path>:<method>"` adds a method to that part's method table on this side
    /// only, so a spawn fails the check with a reason naming the scene and part. Set it before the first
    /// spawn.
    #[var(rename = _debug_extra_method, get = get_debug_extra_method, set = set_debug_extra_method)]
    debug_extra_method: GString,
    /// WP7: the keyed state of this session (`Net.state` is the default session's).
    #[var(get = get_state, no_set)]
    #[init(val = NetKeyedState::new_gd())]
    state: Gd<NetKeyedState>,
    /// WP7: `Net.request_owner` calls waiting for their reply, by entity.
    owner_requests: HashMap<u32, Vec<Gd<NetCall>>>,
    /// WP8: this is `Net`'s session: the meter's toggle key is read here (`_unhandled_input`) and the
    /// editor's debugger tab is fed from here.
    pub(crate) net_default: bool,
    /// WP8: ticks since the editor debugger was last fed.
    debugger_ticks: u32,
    /// WP13: the levels this session loaded (travel.rs).
    pub(crate) levels: Vec<crate::travel::Level>,
    /// WP13, server: queued `Net.travel` requests (applied at the next step).
    pub(crate) travels: Vec<crate::travel::TravelReq>,
    /// WP13, server: a level is unloaded this many seconds after its last player left; 0 =
    /// `Net.empty_level_timeout`, else the project setting `gdnet/levels/empty_timeout_s` (30).
    #[var(get = get_empty_level_timeout, set = set_empty_level_timeout)]
    pub(crate) empty_level_timeout: f64,
    /// WP16, client: room -> level scene path, for each `Travel` without a `RoomLeft` since.
    pub(crate) room_levels: std::collections::BTreeMap<String, String>,
    /// WP16: nodes moved between managers during this step, whose physics tick runs in `post_tick`.
    pub(crate) missed_physics: Vec<Gd<Node>>,
    /// WP16, server: wanted room names that look like scenes but do not exist (warned once).
    pub(crate) not_levels: std::collections::HashSet<String>,
    /// WP14, server: each peer's interacting set as last given to netcore (physics.rs).
    interacting: HashMap<u32, BTreeSet<NetId>>,
    /// WP14: microseconds the last interaction pass took.
    interaction_us: u64,
    /// Review 2026-11-08: interaction passes run so far, and the per-tick mean cost over the last full
    /// second (skipped ticks count as 0): (window sum in µs, window ticks, last mean in ms).
    interaction_passes: u64,
    interaction_window: (u64, u32, f64),
    /// WP14: entities hash telemetry samples per tick on this server; -1 = `Net.hash_sample`, else the
    /// project setting `gdnet/debug/hash_sample` (default 16). Changes apply at once.
    #[var(get = get_hash_sample, set = set_hash_sample)]
    #[init(val = -1)]
    hash_sample: i64,
}

/// WP8: the group every session joins (the overlay and the meter find sessions through it).
pub const SESSIONS_GROUP: &str = "__gdnet_sessions";
/// WP8: an overlay row is red this long after a rejected write.
pub const REJECTED_MS: u64 = 1000;

/// Byte and timing counters behind `stats()` (2.7, the WP8 meter's source).
#[derive(Default, Clone)]
struct Counters {
    sent: [u64; 3],
    received: [u64; 3],
    window_start_sent: [u64; 3],
    window_start_received: [u64; 3],
    sent_per_s: [u64; 3],
    received_per_s: [u64; 3],
    window_ticks: u32,
    poll_us_last: u64,
    poll_us_window: u64,
    poll_ms_avg: f64,
    record_calls: u64,
    /// WP4.
    rollbacks: u64,
    replayed_ticks: u64,
    window_start_rollbacks: u64,
    rollbacks_per_s: u64,
    /// WP6: per-second call counters (sent, received, refused, dropped).
    window_start_calls: [u64; 4],
    calls_per_s: [u64; 4],
    /// WP5.
    window_start_late: u64,
    late_per_s: u64,
    window_start_corrections: u64,
    corrections_per_s: u64,
    /// WP8.
    window_start_divergences: u64,
    divergences_per_s: u64,
    window_start_snapshot_bytes: u64,
    snapshot_bytes_per_s: u64,
    window_start_keyed: u64,
    keyed_bytes_per_s: u64,
    /// WP14: deferred entities and fields per second.
    window_start_deferred: [u64; 2],
    deferred_per_s: [u64; 2],
}

#[godot_api]
impl INode for NetSession {
    fn ready(&mut self) {
        self.base_mut().add_to_group(SESSIONS_GROUP);
        let hotkey = self.net_default;
        self.base_mut().set_process_unhandled_input(hotkey);
        self.base_mut().set_process_priority(-1000);
        self.base_mut().set_physics_process_priority(-1000);
        // Gate G0: no render-frame work unless a client session runs (start() turns it on).
        let client = self
            .core
            .as_ref()
            .is_some_and(|c| c.role() == netcore::Role::Client);
        self.base_mut().set_process(client);
        if !self.poller_added {
            self.poller_added = true;
            let mut poller = NetPoller::new_alloc();
            poller.set_name(POLLER_NODE);
            self.base_mut()
                .add_child_ex(&poller)
                .internal(InternalMode::BACK)
                .done();
        }
    }

    fn physics_process(&mut self, _delta: f64) {
        self.step();
    }

    /// WP8: the meter's toggle (the action `gdnet_toggle_meter` when the project defines it, else F9),
    /// on `Net`'s session only.
    fn unhandled_input(&mut self, event: Gd<godot::classes::InputEvent>) {
        if !self.net_default || !crate::meter::is_toggle(&event) {
            return;
        }
        if let Some(mut vp) = self.base().get_viewport() {
            vp.set_input_as_handled();
        }
        crate::meter::toggle_from_session();
    }

    /// 2.2 step 7 (WP5): interpolated replicas and owned offsets, once per render frame.
    fn process(&mut self, _delta: f64) {
        self.render_frame();
    }

    /// A session whose node leaves the tree ends (as `leave()`): freeing a branch ends its session.
    /// Nodes added to the branch after the session node exit the tree before it, while gdnet's API is
    /// still installed, so they unregister from the API they registered with.
    fn exit_tree(&mut self) {
        self.stop();
    }
}

#[godot_api]
impl NetSession {
    #[func]
    fn get_debug_protocol_salt(&self) -> i64 {
        self.debug_protocol_salt
    }

    #[func]
    fn set_debug_protocol_salt(&mut self, salt: i64) {
        self.debug_protocol_salt = salt;
    }

    #[signal]
    fn peer_joined(id: i64);
    #[signal]
    fn peer_left(id: i64);
    #[signal]
    fn connected();
    #[signal]
    fn disconnected(reason: GString);
    /// Once per physics tick while online (renamed from `tick` in WP2: the method `tick()` shares the
    /// name, so `Net.tick.connect` did not parse).
    #[signal]
    fn ticked(tick: i64);
    /// An entity was spawned in this session's branch (on the authority when announced, on a client
    /// when its replica was added).
    #[signal]
    fn entity_spawned(node: Gd<Node>);
    /// An entity is about to be freed (or its removal was announced).
    #[signal]
    fn entity_despawning(node: Gd<Node>);
    /// A client-side spawn was refused (spawn policy `server_only`); the node is freed.
    #[signal]
    fn spawn_refused(node: Gd<Node>);
    /// WP4: the owner restored the entity `node` to the server's state at `to_tick` and replayed it up to
    /// `from_tick`.
    #[signal]
    fn rolled_back(node: Gd<Node>, from_tick: i64, to_tick: i64);
    /// WP6 (server): a call from `sender` on `node` was refused (D14: the sender does not own the entity
    /// and the part's `_net_allow` did not admit it). The caller's `NetCall` fails.
    #[signal]
    fn call_refused(node: Gd<Node>, method: StringName, sender: i64);

    /// WP7: the entity `node`'s owner changed from `old` to `new` (on every peer, once per change).
    #[signal]
    fn owner_changed(node: Gd<Node>, old: i64, new: i64);

    /// WP7: a write to `node` was rejected. On the server: `sender` wrote state it does not own. On
    /// the writer (`sender` = its own id): the fields were put back to the server's values.
    #[signal]
    fn write_rejected(node: Gd<Node>, sender: i64);

    /// WP13: an entity moved from room `from` to room `to` (`Net.travel`), after its
    /// `_net_travelled(from, to)` ran; on the server and on every client that still sees it.
    #[signal]
    fn travelled(player: Gd<Node>, from: GString, to: GString);

    #[func]
    fn get_empty_level_timeout(&self) -> f64 {
        self.empty_level_timeout
    }

    #[func]
    fn set_empty_level_timeout(&mut self, v: f64) {
        self.empty_level_timeout = v.max(0.0);
    }

    /// WP13/WP16: loads the level scene `scene_path` under the World node (`Net.set_world`, else this
    /// branch's root), named after its file, unless it is loaded; returns its root. `origin` (a
    /// `Vector2` for a 2D level, a `Vector3` for a 3D one) puts the level's root there, the same on
    /// every client; null: on the server the next free cell of the `gdnet/levels/spacing` grid when
    /// that is set (and no manager of the level names `neighbours`), else where it was authored. On
    /// the server the session then uses rooms strictly (a peer without a room sees only `"*"`
    /// managers).
    #[func]
    pub fn load_level(
        &mut self,
        scene_path: GString,
        #[opt(default = net_call::NetArg::absent())] origin: NetArg,
    ) -> Option<Gd<Node>> {
        let place = match crate::world::origin_from_variant(&origin.or_nil()) {
            Ok(Some(o)) => crate::travel::Place::At(o),
            Ok(None) => crate::travel::Place::Auto,
            Err(e) => {
                godot_error!("gdnet: load_level({scene_path}): {e}");
                return None;
            }
        };
        self.do_load_level(&scene_path.to_string(), place)
    }

    /// WP13: frees a loaded level (the server destroys its entities first). False when not loaded.
    #[func]
    pub fn unload_level(&mut self, scene_path: GString) -> bool {
        self.do_unload_level(&scene_path.to_string())
    }

    /// WP13: the scene paths of the levels this session holds, in load order.
    #[func]
    pub fn levels(&mut self) -> PackedStringArray {
        let paths: Vec<GString> = self
            .level_paths()
            .iter()
            .map(|p| GString::from(p.as_str()))
            .collect();
        PackedStringArray::from(&paths[..])
    }

    /// WP16: the origin a loaded level was placed at (a `Vector2` or `Vector3`); null when it sits
    /// where it was authored or is not loaded.
    #[func]
    pub fn level_origin(&mut self, scene_path: GString) -> Variant {
        crate::world::origin_to_variant(self.origin_of_level(&scene_path.to_string()))
    }

    /// WP13/WP16, server: sends the entity `player` belongs to into the level `scene_path` (loaded if
    /// needed), at the node named `spawn_point` inside it; without one it keeps its global position
    /// (a chunk next door). Applied at the next tick.
    #[func]
    pub fn travel(
        &mut self,
        player: Gd<Node>,
        scene_path: GString,
        #[opt(default = "")] spawn_point: GString,
    ) -> bool {
        if !self.is_server() {
            godot_error!("gdnet: Net.travel runs on the server");
            return false;
        }
        self.travels.retain(|r| r.player != player);
        self.travels.push(crate::travel::TravelReq {
            player,
            target: crate::travel::TravelTarget::Scene(scene_path.to_string()),
            spawn_point: spawn_point.to_string(),
        });
        true
    }

    /// WP13, server: puts peer `peer` in room `name` whatever its player's room (empty: back to its
    /// player's room). That room's neighbours and the added rooms stay in its set.
    #[func]
    pub fn set_room(&mut self, peer: i64, name: GString) {
        let name = name.to_string();
        let peer = peer.max(0) as u32;
        self.with_core(|c, now| c.set_room(peer, (!name.is_empty()).then_some(name.as_str()), now));
    }

    /// WP16, server: peer `peer` also sees room `name` (a level's room is its scene path: the server
    /// loads it if needed, and the peer's client loads it too). False when it was added already.
    #[func]
    pub fn add_room(&mut self, peer: i64, name: GString) -> bool {
        let name = name.to_string();
        let peer = peer.max(0) as u32;
        let added = self
            .with_core(|c, now| c.add_room(peer, &name, now))
            .unwrap_or(false);
        if added && self.is_server() {
            self.load_wanted_levels();
        }
        added
    }

    /// WP16, server: undoes `add_room`; the peer's client frees that level. False when not added.
    #[func]
    pub fn remove_room(&mut self, peer: i64, name: GString) -> bool {
        let name = name.to_string();
        let peer = peer.max(0) as u32;
        self.with_core(|c, now| c.remove_room(peer, &name, now))
            .unwrap_or(false)
    }

    /// WP13, server: the entity `node` belongs to is `peer`'s player (its manager's room is the peer's
    /// room). Before it is spawned, it becomes the player when it is.
    #[func]
    pub fn set_player(&mut self, peer: i64, node: Gd<Node>) -> bool {
        let peer = peer.max(0) as u32;
        let Some((root, m)) = entity_root_of(&node) else {
            let mut n = node;
            n.set_meta(crate::manager::META_PLAYER, &2i64.to_variant());
            return true;
        };
        let id = m.bind().net_id_of(&root);
        if id == 0 {
            let mut r = root;
            r.set_meta(crate::manager::META_PLAYER, &2i64.to_variant());
            return true;
        }
        self.with_core(|c, now| c.set_player(peer, id, now))
            .unwrap_or(false)
    }

    /// WP13: the primary room of peer `peer` (its player's, or the one `set_room` gave it): on the
    /// server for any peer ("" without one); on a client for its own id (the room of its first
    /// entity).
    #[func]
    pub fn room_of(&self, peer: i64) -> GString {
        let Some(core) = self.core.as_ref() else {
            return GString::new();
        };
        let room = if core.role() == netcore::Role::Server {
            core.peer_room(peer.max(0) as u32).unwrap_or_default()
        } else if peer == i64::from(core.local_id()) {
            self.client_room()
        } else {
            String::new()
        };
        GString::from(room.as_str())
    }

    /// WP16: every room of peer `peer`: on the server its primary room first, then its neighbours and
    /// added rooms; on a client, for its own id, the rooms whose levels it holds.
    #[func]
    pub fn rooms(&self, peer: i64) -> PackedStringArray {
        let Some(core) = self.core.as_ref() else {
            return PackedStringArray::new();
        };
        let rooms = if core.role() == netcore::Role::Server {
            core.peer_rooms(peer.max(0) as u32)
        } else if peer == i64::from(core.local_id()) {
            self.client_rooms()
        } else {
            Vec::new()
        };
        let names: Vec<GString> = rooms.iter().map(|r| GString::from(r.as_str())).collect();
        PackedStringArray::from(&names[..])
    }

    /// WP13: the node levels are loaded under (the World `Net.set_world` registered in this branch, else
    /// the branch root).
    #[func]
    pub fn get_world(&self) -> Option<Gd<Node>> {
        self.world()
    }

    /// WP7: the keyed state (`NetKeyedState`) of this session.
    #[func]
    pub fn get_state(&self) -> Gd<NetKeyedState> {
        self.state.clone()
    }

    /// WP6 (rung 2): runs `callable` (a method of a part of a managed entity) on the authority, then on
    /// every peer that has the entity in scope. From a client it goes to the server only, which checks
    /// that the caller owns the entity (or the part's `_net_allow(method, sender)` admits it), runs it
    /// and sends it to everyone, the caller included. Up to eight arguments; bound arguments count.
    /// Returns a `NetCall` (`ok` / `error` once `completed`). Without a session it just runs.
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn sync(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Sync,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: `sync` on the unreliable channel (no answer, may be lost; for cosmetic effects).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn sync_unreliable(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Sync,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            true,
        )
    }

    /// WP6: runs `callable` on the server (any peer may call it; the method may take a trailing
    /// `NetCallInfo` to learn the sender) and returns the `completed` signal of its `NetCall`, so
    /// `var v = await Net.server(try_buy, 3)` is the server's return value (null on an error; the
    /// signal's object is the `NetCall`).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn server(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Signal {
        let call = net_call::make_call(
            Verb::Server,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        );
        Signal::from_object_signal(&call, "completed")
    }

    /// WP6: runs `callable` on the entity's owner only.
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn owner(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Owner,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: runs `callable` on every peer but the caller (the server included when a client calls).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn others(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Others,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: runs `callable` on peer `id` only (1 = the server; a client's call goes via the server).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn peer(
        &mut self,
        id: i64,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Peer,
            id.max(0) as u32,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    #[func]
    fn get_debug_extra_method(&self) -> GString {
        self.debug_extra_method.clone()
    }

    #[func]
    fn set_debug_extra_method(&mut self, v: GString) {
        self.debug_extra_method = v;
    }

    #[func]
    fn get_budget_bytes_per_s(&self) -> i64 {
        self.budget_bytes_per_s
    }

    #[func]
    fn set_budget_bytes_per_s(&mut self, v: i64) {
        self.budget_bytes_per_s = v.max(0);
        let b = self.effective_budget();
        if let Some(c) = self.core.as_mut() {
            c.set_budget(b);
        }
    }

    #[func]
    fn get_hash_sample(&self) -> i64 {
        self.hash_sample
    }

    #[func]
    fn set_hash_sample(&mut self, v: i64) {
        self.hash_sample = v.max(-1);
        let n = self.effective_hash_sample();
        if let Some(c) = self.core.as_mut() {
            let on = c.hash_telemetry();
            c.set_hash_telemetry(on, n);
        }
    }

    /// WP14, server: the net ids marked as interacting with `peer` this tick (near or touching its
    /// player), ascending; empty on a client.
    #[func]
    pub fn interacting(&self, peer: i64) -> PackedInt64Array {
        let ids = self
            .core
            .as_ref()
            .map(|c| c.interacting(peer.max(0) as u32))
            .unwrap_or_default();
        ids.into_iter().map(i64::from).collect()
    }

    /// WP14, server: how many snapshots carried the entity `node` belongs to to `peer` (0 on a client
    /// or for an unmanaged node).
    #[func]
    pub fn entity_sends(&self, node: Gd<Node>, peer: i64) -> i64 {
        let Some(id) = entity_root_of(&node).map(|(root, m)| m.bind().net_id_of(&root)) else {
            return 0;
        };
        self.core
            .as_ref()
            .map_or(0, |c| c.entity_sends(peer.max(0) as u32, id) as i64)
    }

    #[func]
    fn get_cold_poll_ticks(&self) -> i64 {
        self.cold_poll_ticks
    }

    #[func]
    fn set_cold_poll_ticks(&mut self, v: i64) {
        self.cold_poll_ticks = v.max(0);
    }

    #[func]
    fn get_debug_perturb_property(&self) -> GString {
        self.debug_perturb_property.clone()
    }

    #[func]
    fn set_debug_perturb_property(&mut self, v: GString) {
        self.debug_perturb_property = v;
    }

    /// Counters for the meter (WP8) and tests: `bytes_sent_per_s` / `bytes_received_per_s` (per channel,
    /// the last full second), `bytes_sent_total` / `bytes_received_total` (per channel),
    /// `entities`, `poll_ms` (the last record step), `poll_ms_avg` (over the last second),
    /// `budget_bytes_per_s`, `cold_poll_ticks`, and netcore's snapshot counters (`snapshots_sent`,
    /// `snapshot_bytes`, `snapshot_entities_sent`, `snapshot_fields_sent`, `reliable_fields_sent`,
    /// `snapshots_received`, `snapshots_stale`, `snapshots_unacked`, `fields_received`, `fallbacks`,
    /// `changes`). WP4: `rollbacks` (total), `rollbacks_per_s` (the last full second),
    /// `replayed_ticks`, `inputs_repeated` (server: ticks a client's last frame was repeated),
    /// `input_frames_received`, `input_messages_sent`, `prediction_confirmations`,
    /// `prediction_mismatches`, `prediction_stale`, `prediction_lead` (ticks; -1 unknown),
    /// `input_delay` (effective ticks), `predicted_tick`. WP5: `jitter_buffer_ticks` (effective),
    /// `render_lag_ticks` (newest snapshot tick minus the render tick), `late_snapshots` (total) and
    /// `late_snapshots_per_s`, `offset_corrections` (corrections turned into decaying offsets, replicas
    /// and owned blends) and `offset_corrections_per_s`, `interp_teleports`.
    #[func]
    pub fn stats(&self) -> VarDictionary {
        let c = &self.stats;
        let arr = |a: &[u64; 3]| PackedInt64Array::from(&a.map(|x| x as i64)[..]);
        let mut d = VarDictionary::new();
        d.set("bytes_sent_per_s", &arr(&c.sent_per_s));
        d.set("bytes_received_per_s", &arr(&c.received_per_s));
        d.set("bytes_sent_total", &arr(&c.sent));
        d.set("bytes_received_total", &arr(&c.received));
        d.set("entities", self.entity_count());
        d.set("poll_ms", c.poll_us_last as f64 / 1000.0);
        d.set("poll_ms_avg", c.poll_ms_avg);
        d.set("budget_bytes_per_s", i64::from(self.effective_budget()));
        d.set("cold_poll_ticks", i64::from(self.effective_cold_poll()));
        if let Some(core) = self.core.as_ref() {
            let r = core.repl_stats();
            d.set("snapshots_sent", r.snapshots_sent as i64);
            d.set("snapshot_bytes", r.snapshot_bytes as i64);
            d.set("snapshot_entities_sent", r.entities_sent as i64);
            d.set("snapshot_fields_sent", r.fields_sent as i64);
            d.set("reliable_fields_sent", r.reliable_fields_sent as i64);
            d.set("snapshots_received", r.snapshots_received as i64);
            d.set("snapshots_stale", r.snapshots_stale as i64);
            d.set("snapshots_unacked", r.snapshots_unacked as i64);
            d.set("fields_received", r.fields_received as i64);
            d.set("fallbacks", r.fallbacks as i64);
            d.set("changes", r.changes as i64);
            // WP14: budget degradation and interaction scoring.
            d.set("deferred_entities", r.deferred_entities as i64);
            d.set("deferred_fields", r.deferred_fields as i64);
            d.set("deferred_entities_tick", r.deferred_entities_tick as i64);
            d.set("deferred_fields_tick", r.deferred_fields_tick as i64);
            d.set("deferred_entities_per_s", c.deferred_per_s[0] as i64);
            d.set("deferred_fields_per_s", c.deferred_per_s[1] as i64);
            let n: usize = self.interacting.values().map(BTreeSet::len).sum();
            d.set("interacting", n as i64);
            d.set("interaction_ms", self.interaction_us as f64 / 1000.0);
            d.set("interaction_ms_avg", self.interaction_window.2);
            d.set("interaction_passes", self.interaction_passes as i64);
            d.set("hash_sample", i64::from(self.effective_hash_sample()));
            let i = core.input_stats();
            let p = core.predict_stats();
            d.set("inputs_repeated", i.repeated as i64);
            d.set("input_frames_received", i.frames_received as i64);
            d.set("input_messages_sent", i.messages_sent as i64);
            d.set("prediction_confirmations", p.confirmations as i64);
            d.set("prediction_mismatches", p.mismatches as i64);
            d.set("prediction_stale", p.stale as i64);
            d.set("prediction_lead", core.prediction_lead().unwrap_or(-1));
            d.set("input_delay", i64::from(core.input_delay()));
            d.set("predicted_tick", core.predicted_tick() as i64);
        }
        d.set("rollbacks", c.rollbacks as i64);
        d.set("rollbacks_per_s", c.rollbacks_per_s as i64);
        d.set("replayed_ticks", c.replayed_ticks as i64);
        d.set("jitter_buffer_ticks", self.get_jitter_buffer_ticks());
        let (lag, i) = self
            .core
            .as_ref()
            .map_or((0.0, Default::default()), |core| {
                (
                    core.interp().timeline.render_lag(),
                    core.interp().timeline.stats,
                )
            });
        let i: netcore::InterpStats = i;
        d.set("render_lag_ticks", lag);
        d.set("late_snapshots", i.late_snapshots as i64);
        d.set("late_snapshots_per_s", c.late_per_s as i64);
        d.set("offset_corrections", i.corrections as i64);
        d.set("offset_corrections_per_s", c.corrections_per_s as i64);
        d.set("interp_teleports", i.teleports as i64);
        let cs = self
            .core
            .as_ref()
            .map(|c| c.call_stats())
            .unwrap_or_default();
        d.set("calls_sent", cs.sent as i64);
        d.set("calls_received", cs.received as i64);
        d.set("calls_refused", cs.refused as i64);
        d.set("calls_dropped", cs.dropped as i64);
        d.set("calls_unknown_method", cs.unknown_method as i64);
        d.set("calls_timed_out", cs.timeouts as i64);
        d.set("calls_pending", self.calls.len() as i64);
        d.set("calls_sent_per_s", c.calls_per_s[0] as i64);
        d.set("calls_received_per_s", c.calls_per_s[1] as i64);
        d.set("calls_refused_per_s", c.calls_per_s[2] as i64);
        d.set("calls_dropped_per_s", c.calls_per_s[3] as i64);
        // WP7.
        if let Some(core) = self.core.as_ref() {
            let a = core.authority_stats();
            d.set("owner_changes", a.owner_changes as i64);
            d.set("writes_rejected", a.writes_rejected as i64);
            d.set("writes_sent", a.writes_sent as i64);
            d.set("owner_states_sent", a.owner_states_sent as i64);
            d.set("owner_states_received", a.owner_states_received as i64);
            d.set("ownership_requests_granted", a.requests_granted as i64);
            d.set("ownership_requests_refused", a.requests_refused as i64);
            let k = core.keyed().stats;
            d.set("keyed_bytes_sent", k.bytes_sent as i64);
            d.set("keyed_bytes_received", k.bytes_received as i64);
            d.set("keyed_writes_rejected", k.rejected as i64);
            d.set("keyed_entries", core.keyed().len() as i64);
        }
        // WP8.
        self.diagnostics_into(&mut d);
        d
    }

    /// WP8: the divergences hash telemetry counted for the entity `node` belongs to on this client (0
    /// on the server, for an unmanaged node, or without a session).
    #[func]
    pub fn divergences(&self, node: Gd<Node>) -> i64 {
        let Some(id) = entity_root_of(&node).map(|(root, m)| m.bind().net_id_of(&root)) else {
            return 0;
        };
        if id == 0 {
            return 0;
        }
        self.core
            .as_ref()
            .map_or(0, |c| c.divergences_of(id) as i64)
    }

    /// WP8 test hook: replaces this client's held copy of the replicated property `property`
    /// (`prop` or `Part:prop`) of the entity `node` belongs to, as if a snapshot had been applied
    /// wrongly; hash telemetry then counts divergences for it. Returns false when the property is not
    /// replicated or this is not a client.
    #[func]
    fn _debug_corrupt_held(&mut self, node: Gd<Node>, property: GString, value: Variant) -> bool {
        let Some((root, m)) = entity_root_of(&node) else {
            return false;
        };
        let text = property.to_string();
        let (part, prop) = match text.rsplit_once(':') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (String::new(), text.clone()),
        };
        let Some((id, part)) = m.bind().part_address(&root, &part) else {
            return false;
        };
        let Some(core) = self.core.as_mut() else {
            return false;
        };
        let Some(scene) = core.entities().get(id).map(|r| r.scene) else {
            return false;
        };
        let Some((field, ty)) = core.props(scene).and_then(|t| {
            t.parts
                .get(usize::from(part))?
                .iter()
                .position(|d| d.name == prop)
                .map(|f| (f as u16, t.parts[usize::from(part)][f].ty))
        }) else {
            return false;
        };
        let v = crate::apply::to_value(&value, ty);
        core.debug_corrupt_held(id, part, field, &v);
        core.role() == netcore::Role::Client
    }

    /// WP8: every managed entity of this session's branch as the ownership overlay sees it, one
    /// Dictionary each: `net_id`, `owner`, `role` (`"rejected"` = a write was rejected in the last
    /// second, red; `"client_owned"` = the owner's state is trusted, amber; `"server"` = server-owned,
    /// white; `"own"` = owned by this peer, green; `"other"` = another client's, grey), `color`,
    /// `client_owned`, `transfer` (0 fixed, 1 request, 2 take), `scene`, `parts`, `hashes_diverged`
    /// and `node`.
    #[func]
    pub fn overlay_rows(&self) -> VarArray {
        let mut out = VarArray::new();
        if self.core.is_none() {
            return out;
        }
        let local = self.core.as_ref().map_or(0, |c| c.local_id());
        let now = Time::singleton().get_ticks_msec();
        let mut seen = std::collections::HashSet::new();
        for (_, m) in self.managers() {
            let room = GString::from(self.room_name(&m).as_str());
            for e in m.bind().overlay_entries() {
                if !seen.insert(e.id) {
                    continue;
                }
                let role = crate::overlay::role_of(&e, local, now);
                let mut d = VarDictionary::new();
                d.set("net_id", i64::from(e.id));
                d.set("owner", i64::from(e.owner));
                d.set("role", role);
                d.set("color", crate::overlay::color_of(role));
                d.set("client_owned", e.client_owned);
                d.set("transfer", e.transfer as i64);
                d.set("scene", &GString::from(e.scene.as_str()));
                d.set("parts", i64::from(e.parts));
                d.set(
                    "hashes_diverged",
                    self.core
                        .as_ref()
                        .map_or(0, |c| c.divergences_of(e.id) as i64),
                );
                d.set("node", &e.node);
                d.set("room", &room);
                out.push(&d.to_variant());
            }
        }
        out
    }

    /// WP5: the effective jitter buffer in ticks: a client's current (adaptive) value, else the
    /// project setting `gdnet/interpolation/jitter_buffer_ticks` (default 2).
    #[func]
    pub fn get_jitter_buffer_ticks(&self) -> i64 {
        i64::from(
            self.core
                .as_ref()
                .filter(|c| c.role() == netcore::Role::Client)
                .map_or_else(interp::jitter_buffer_ticks, |c| {
                    c.interp().timeline.buffer()
                }),
        )
    }

    #[func]
    fn get_debug_part_table_salt(&self) -> i64 {
        self.debug_part_table_salt
    }

    #[func]
    fn set_debug_part_table_salt(&mut self, salt: i64) {
        self.debug_part_table_salt = salt;
    }

    /// Packets all gdnet sessions of this process have sent (gate G0: 0 while nothing uses gdnet).
    #[func]
    pub fn packets_sent_total() -> i64 {
        PACKETS_SENT.load(Ordering::Relaxed) as i64
    }

    /// Entities in this session's table (every manager of the branch).
    #[func]
    pub fn entity_count(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| c.entities().len() as i64)
    }

    /// Starts a session over `peer`, installing gdnet's `MultiplayerAPI` for `root_path` (empty: the
    /// whole tree). A peer whose unique id is 1 hosts; any other joins. Returns `OK` or an error code.
    #[func]
    pub fn start(&mut self, peer: Gd<MultiplayerPeer>, root_path: NodePath) -> Error {
        if self.core.is_some() {
            self.leave();
        }
        let Some(mut tree) = scene_tree() else {
            godot_error!("NetSession.start: no SceneTree");
            return Error::ERR_UNCONFIGURED;
        };
        let default_api = tree.get_multiplayer();
        let (inner, restore) = if root_path.is_empty() {
            let Some(api) = default_api else {
                return Error::ERR_UNCONFIGURED;
            };
            match api.clone().try_cast::<SceneMultiplayer>() {
                Ok(sm) => {
                    let previous_peer = sm.get_multiplayer_peer();
                    (
                        sm,
                        Restore::Default {
                            api,
                            peer: previous_peer,
                        },
                    )
                }
                Err(_) => (
                    SceneMultiplayer::new_gd(),
                    Restore::Default { api, peer: None },
                ),
            }
        } else {
            let current = tree.get_multiplayer_ex().for_path(&root_path).done();
            let is_default = match (&current, &default_api) {
                (Some(a), Some(b)) => a.instance_id() == b.instance_id(),
                (None, _) => true,
                _ => false,
            };
            if is_default {
                (
                    SceneMultiplayer::new_gd(),
                    Restore::Branch {
                        path: root_path.clone(),
                        previous: None,
                    },
                )
            } else {
                let previous = current.clone();
                let sm = current
                    .and_then(|c| c.try_cast::<SceneMultiplayer>().ok())
                    .unwrap_or_else(SceneMultiplayer::new_gd);
                (
                    sm,
                    Restore::Branch {
                        path: root_path.clone(),
                        previous,
                    },
                )
            }
        };

        let me = self.to_gd();
        let mut inner_mut = inner.clone();
        for (signal, method) in HOOKS {
            let cb = Callable::from_object_method(&me, method);
            if !inner_mut.is_connected(signal, &cb) {
                inner_mut.connect(signal, &cb);
            }
        }
        inner_mut.set_multiplayer_peer(&peer);
        let mut api = NetMultiplayerApi::wrap(inner.clone());
        api.set_meta(META_SESSION, &me.instance_id().to_i64().to_variant());
        self.branch_root = if root_path.is_empty() {
            tree.get_root().map(|r| r.upcast())
        } else {
            tree.get_root().and_then(|r| r.get_node_or_null(&root_path))
        };
        self.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        self.unrouted.clear();
        if root_path.is_empty() {
            tree.set_multiplayer(&api.clone().upcast::<MultiplayerApi>());
        } else {
            tree.set_multiplayer_ex(&api.clone().upcast::<MultiplayerApi>())
                .root_path(&root_path)
                .done();
        }

        let mut cfg = SessionConfig::new(tick_rate());
        cfg.protocol_salt = self.debug_protocol_salt as u64;
        cfg.profiles_hash = profiles::global_profiles().hash();
        cfg.budget_bytes_per_s = self.effective_budget();
        cfg.max_rollback_ticks = tick::max_rollback_ticks(cfg.tick_rate);
        let hosting = peer.get_unique_id() == 1;
        let mut core = if hosting {
            Session::server(cfg)
        } else {
            Session::client(cfg)
        };
        core.set_interpolation(
            interp::jitter_buffer_ticks(),
            interp::adaptive(),
            interp::teleport_distance(),
        );
        core.set_hash_telemetry(crate::hash::telemetry(), self.effective_hash_sample());
        core.set_hash_refresh(crate::hash::refresh_s());
        let now = now_us();
        if hosting {
            for id in inner.get_peers().as_slice() {
                core.peer_connected(*id as u32, now);
            }
        } else if peer.get_connection_status() == ConnectionStatus::CONNECTED {
            core.peer_connected(1, now);
        }
        self.core = Some(core);
        self.api = Some(api);
        self.inner = Some(inner);
        self.peer = Some(peer);
        self.restore = Some(restore);
        self.inbox.clear();
        self.applied = None;
        self.stats = Counters::default();
        self.last_ptick = None;
        self.owned_active = false;
        let iid = me.instance_id();
        self.state.bind_mut().attach(Some(iid));
        if !hosting {
            let cb = Callable::from_object_method(&me, PHYSICS_FRAME_HOOK.1);
            if !tree.is_connected(PHYSICS_FRAME_HOOK.0, &cb) {
                tree.connect(PHYSICS_FRAME_HOOK.0, &cb);
            }
        }
        self.apply_emulation();
        self.base_mut().set_physics_process(true);
        self.base_mut().set_process(!hosting);
        Error::OK
    }

    /// Ends the session: closes the peer and puts back the `MultiplayerAPI` (and peer) that was there.
    /// Emits nothing.
    #[func]
    pub fn leave(&mut self) {
        self.stop();
    }

    /// True from `start()` until `leave()` or a disconnection.
    #[func]
    pub fn is_online(&self) -> bool {
        self.core.as_ref().is_some_and(|c| !c.is_closed())
    }

    /// True while hosting (the peer's id is 1).
    #[func]
    pub fn is_server(&self) -> bool {
        self.core
            .as_ref()
            .is_some_and(|c| c.role() == netcore::Role::Server)
    }

    /// This side's peer id: 1 when hosting, the server-assigned id on a client after `connected`, 0
    /// otherwise.
    #[func]
    pub fn unique_id(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| i64::from(c.local_id()))
    }

    /// The current tick (0 when offline). Named `tick` in GDScript; the signal is `ticked`.
    #[func(rename = tick)]
    pub fn current_tick(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| c.current_tick() as i64)
    }

    /// Smoothed round-trip time to `peer` in milliseconds; -1 when unknown.
    #[func]
    pub fn rtt(&self, peer: i64) -> f64 {
        self.core
            .as_ref()
            .and_then(|c| c.rtt_ms(peer as u32))
            .unwrap_or(-1.0)
    }

    /// RTT jitter (mean deviation) to `peer` in milliseconds; -1 when unknown.
    #[func]
    pub fn jitter(&self, peer: i64) -> f64 {
        self.core
            .as_ref()
            .and_then(|c| c.jitter_ms(peer as u32))
            .unwrap_or(-1.0)
    }

    /// The remote peers that finished the handshake, ascending (on a client: `[1]` once connected).
    #[func]
    pub fn peers(&self) -> PackedInt32Array {
        let ids: Vec<i32> = self.core.as_ref().map_or_else(Vec::new, |c| {
            c.joined_peers().into_iter().map(|p| p as i32).collect()
        });
        PackedInt32Array::from(ids.as_slice())
    }

    /// The estimated server tick (fractional); on the server, its own tick.
    #[func]
    pub fn server_tick(&self) -> f64 {
        self.core
            .as_ref()
            .map_or(0.0, |c| c.clock().server_tick_estimate())
    }

    /// Physics ticks per second: the session's tick rate.
    #[func]
    pub fn get_tick_rate(&self) -> i64 {
        self.core
            .as_ref()
            .map_or_else(|| i64::from(tick_rate()), |c| i64::from(c.tick_rate()))
    }

    /// gdnet's `MultiplayerAPI` while online (the one `SceneTree.get_multiplayer()` returns for the
    /// session's branch).
    #[func]
    pub fn get_api(&self) -> Option<Gd<NetMultiplayerApi>> {
        self.api.clone()
    }

    /// The transport peer while online.
    #[func]
    pub fn get_peer(&self) -> Option<Gd<MultiplayerPeer>> {
        self.peer.clone()
    }

    /// Emulation for this session: applied by the `NetPipePeer` when the peer is one, else by netcore
    /// to gdnet's own messages. Null for none.
    #[func]
    pub fn set_emulation(&mut self, emulation: Option<Gd<NetEmulation>>) {
        self.emulation = emulation;
        self.applied = None;
        self.apply_emulation();
    }

    #[func]
    pub fn get_emulation(&self) -> Option<Gd<NetEmulation>> {
        self.emulation.clone()
    }

    #[func]
    fn _on_peer_packet(&mut self, id: i64, packet: PackedByteArray) {
        if self.core.is_some() {
            if let Some(&k) = packet.as_slice().first() {
                let ch = usize::from(netcore::wire::channel_of_kind(k).index()).min(2);
                self.stats.received[ch] += packet.len() as u64;
            }
            self.inbox.push((id as i32, packet.to_vec()));
        }
    }

    /// WP5: the start of a physics frame: the owned offsets the last render frame showed come off.
    #[func]
    fn _on_physics_frame(&mut self) {
        self.restore_owned_offsets();
    }

    #[func]
    fn _on_peer_connected(&mut self, id: i64) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_connected(id as u32, now);
        }
    }

    #[func]
    fn _on_peer_disconnected(&mut self, id: i64) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_disconnected(id as u32, now);
        }
    }

    #[func]
    fn _on_connected_to_server(&mut self) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_connected(1, now);
        }
    }

    #[func]
    fn _on_connection_failed(&mut self) {
        if let Some(c) = self.core.as_mut() {
            c.connection_failed("could not connect to the server");
        }
    }

    #[func]
    fn _on_server_disconnected(&mut self) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_disconnected(1, now);
        }
    }
}

impl NetSession {
    /// One physics tick: 2.2 step 1 (drain), netcore's tick, entity actions and received state, then
    /// the signals. Steps 5 and 6 (record, send) run in `post_tick()`, from the `NetPoller` at +1000.
    fn step(&mut self) {
        if self.core.is_none() {
            return;
        }
        // WP5: the owned offsets come off before anything simulates (normally already done at the
        // tree's `physics_frame`).
        self.restore_owned_offsets();
        self.apply_emulation();
        let now = now_us();
        let inbox = std::mem::take(&mut self.inbox);
        {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            for (peer, bytes) in inbox {
                core.receive(peer as u32, &bytes, now);
            }
            core.tick(now);
        }
        self.resolve_calls();
        self.sync_managers();
        if self.is_server() && self.is_connected() {
            // WP13: travels queued since the last step (WP16: auto-travel queues its own first),
            // then the empty-level timer.
            self.check_auto_travel();
            self.process_travels();
            self.check_empty_levels();
        }
        self.input_step();
        self.keyed_step();
        let (events, tick) = {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            let mut events = Vec::new();
            while let Some(e) = core.poll_event() {
                events.push(e);
            }
            (events, core.current_tick() as i64)
        };
        let mut ended: Option<String> = None;
        for e in events {
            match e {
                Event::PeerJoined(id) => {
                    self.base_mut()
                        .emit_signal("peer_joined", &[i64::from(id).to_variant()]);
                }
                Event::PeerLeft(id) => {
                    self.base_mut()
                        .emit_signal("peer_left", &[i64::from(id).to_variant()]);
                }
                Event::Connected { .. } => {
                    self.base_mut().emit_signal("connected", &[]);
                }
                Event::DisconnectPeer { peer, .. } => {
                    if let Some(mut p) = self.peer.clone() {
                        let _guard = self.base_mut();
                        p.disconnect_peer(peer as i32);
                    }
                }
                Event::Disconnected(reason) => ended = Some(reason),
                Event::EntityError(text) => godot_error!("{text}"),
            }
        }
        if let Some(reason) = ended {
            self.stop();
            self.base_mut().emit_signal(
                "disconnected",
                &[GString::from(reason.as_str()).to_variant()],
            );
            return;
        }
        self.base_mut().emit_signal("ticked", &[tick.to_variant()]);
    }

    /// 2.2 steps 5 and 6, from the `NetPoller` (priority +1000): the server records its managers'
    /// entities (hot properties every tick, cold ones every `cold_poll_ticks`, staggered), then every
    /// side builds its snapshots / acks and hands netcore's packets to the transport.
    pub fn post_tick(&mut self) {
        if self.core.is_none() {
            return;
        }
        let now = now_us();
        self.refresh_live_settings();
        // WP16: the tick of nodes travel moved during the physics pass (travel.rs).
        self.catch_up_moved();
        if self.is_server() && self.is_connected() {
            let t0 = now_us();
            let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
            let cold = self.effective_cold_poll();
            let managers = self.managers();
            let mut batch = Vec::new();
            {
                let _guard = self.base_mut();
                for (_, mut m) in managers {
                    m.bind_mut().record(tick, cold, &mut batch);
                }
            }
            if let Some(core) = self.core.as_mut() {
                for (id, part, values) in &batch {
                    core.record_part(*id, *part, values);
                }
            }
            let us = now_us().saturating_sub(t0);
            self.stats.poll_us_last = us;
            self.stats.poll_us_window += us;
            self.stats.record_calls += 1;
            self.interaction_pass();
        }
        if !self.is_server() && self.is_connected() {
            // WP4: the owner records its predicted entities' hot state and queues its sampled frames.
            let managers = self.managers();
            let (mut hist, mut frames) = (Vec::new(), Vec::new());
            {
                let _guard = self.base_mut();
                for (_, mut m) in managers {
                    m.bind_mut().record_owned(&mut hist, &mut frames);
                }
            }
            if let Some(core) = self.core.as_mut() {
                let p = core.predicted_tick();
                for (id, values) in hist {
                    core.record_prediction(id, p, values);
                }
                for (id, f) in frames {
                    core.local_input(id, f);
                }
            }
            // WP7: the state of the client_owned entities this client owns, and writes to replicas.
            let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
            let cold = self.effective_cold_poll();
            let managers = self.managers();
            let (mut owned, mut polled) = (Vec::new(), Vec::new());
            {
                let _guard = self.base_mut();
                for (_, mut m) in managers {
                    let mut b = m.bind_mut();
                    b.record_client_owned(tick, cold, &mut owned);
                    b.poll_writes(tick, cold, &mut polled);
                }
            }
            if let Some(core) = self.core.as_mut() {
                for (id, part, values) in &owned {
                    core.record_part(*id, *part, values);
                }
                for (id, part, values) in &polled {
                    core.check_writes(*id, *part, values);
                }
            }
        }
        let outgoing = match self.core.as_mut() {
            Some(core) => {
                core.send_snapshots(now);
                core.outgoing()
            }
            None => return,
        };
        self.flush(outgoing);
        self.roll_stats_window();
        self.feed_debugger();
    }

    /// WP8: the meter's and the editor tab's figures beyond the counters above.
    fn diagnostics_into(&self, d: &mut VarDictionary) {
        let c = &self.stats;
        let Some(core) = self.core.as_ref() else {
            d.set("hash_telemetry", crate::hash::telemetry());
            return;
        };
        let h = core.hash_stats();
        d.set("hash_telemetry", core.hash_telemetry());
        d.set("hashes_sent", h.sent as i64);
        d.set("hashes_received", h.received as i64);
        d.set("hashes_compared", h.compared as i64);
        d.set("hashes_skipped", h.skipped as i64);
        d.set("divergences", h.divergences as i64);
        d.set("divergences_per_s", c.divergences_per_s as i64);
        let peers = core.joined_peers();
        let avg = |f: &dyn Fn(u32) -> Option<f64>| {
            let v: Vec<f64> = peers.iter().filter_map(|p| f(*p)).collect();
            if v.is_empty() {
                -1.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        d.set("rtt_ms", avg(&|p| core.rtt_ms(p)));
        d.set("jitter_ms", avg(&|p| core.jitter_ms(p)));
        let tick = core.current_tick();
        d.set("tick", tick as i64);
        let offset = if core.role() == netcore::Role::Client {
            core.clock().server_tick_estimate() - tick as f64
        } else {
            0.0
        };
        d.set("server_tick_offset", offset);
        let server = core.role() == netcore::Role::Server;
        let snap = if server {
            c.snapshot_bytes_per_s
        } else {
            c.received_per_s[1]
        };
        d.set("snapshot_bytes_per_s", snap as i64);
        d.set("keyed_bytes_per_s", c.keyed_bytes_per_s as i64);
        // WP13: a client's scope is what it holds; the server sees everything and reports each peer's.
        d.set("entities_in_scope", core.entities().len() as i64);
        let mut by_peer = VarDictionary::new();
        if server {
            for p in &peers {
                by_peer.set(i64::from(*p), core.scope_len(*p) as i64);
            }
        }
        d.set("entities_in_scope_by_peer", &by_peer);
        // WP16: this peer's primary room and its whole set (on the server: the host player's).
        let (local_room, my_rooms) = if server {
            (
                core.peer_room(netcore::SERVER).unwrap_or_default(),
                core.peer_rooms(netcore::SERVER),
            )
        } else {
            (self.client_room(), self.client_rooms())
        };
        d.set("room", &GString::from(local_room.as_str()));
        let my_rooms: Vec<GString> = my_rooms.iter().map(|r| GString::from(r.as_str())).collect();
        d.set("my_rooms", &PackedStringArray::from(&my_rooms[..]));
        let mut rooms = VarDictionary::new();
        for (key, m) in self.managers() {
            let name = core.manager_room(key).unwrap_or_else(|| self.room_name(&m));
            let n = m.bind().entity_count_pub();
            let prev = rooms
                .get(name.as_str())
                .and_then(|v| v.try_to::<i64>().ok())
                .unwrap_or(0);
            rooms.set(name.as_str(), prev + n);
        }
        d.set("rooms", &rooms);
        d.set("levels", self.levels.len() as i64);
        d.set(
            "travelling",
            if server {
                core.travelling() as i64
            } else {
                // WP16: moves into a level that is not loaded yet.
                self.unrouted
                    .iter()
                    .filter(|(_, _, e)| crate::travel::is_cross_move(e).is_some())
                    .count() as i64
            },
        );
        let sc = core.scope_stats();
        d.set("travels", (sc.travels_sent + sc.travels_received) as i64);
        d.set("scope_left", (sc.left + sc.freed) as i64);
        d.set("scope_entered", sc.entered as i64);
        let budget = f64::from(core.budget()) * peers.len().max(1) as f64;
        d.set(
            "budget_use",
            if server {
                c.snapshot_bytes_per_s as f64 / budget
            } else {
                0.0
            },
        );
    }

    /// WP8: twice a second, `Net`'s session sends `gdnet:stats` (the `stats()` Dictionary) and
    /// `gdnet:entities` (`overlay_rows()` without the nodes) to the editor's debugger when the game runs
    /// under it.
    fn feed_debugger(&mut self) {
        if !self.net_default {
            return;
        }
        self.debugger_ticks += 1;
        if self.debugger_ticks < (tick_rate() / 2).max(1) {
            return;
        }
        self.debugger_ticks = 0;
        let mut dbg = godot::classes::EngineDebugger::singleton();
        if !dbg.is_active() {
            return;
        }
        let stats = self.stats();
        let mut rows = self.overlay_rows();
        for i in 0..rows.len() {
            if let Ok(mut d) = rows.at(i).try_to::<VarDictionary>() {
                d.remove("node");
                rows.set(i, &d.to_variant());
            }
        }
        let mut a = VarArray::new();
        a.push(&stats.to_variant());
        dbg.send_message(crate::editor::MSG_STATS, &a);
        let mut b = VarArray::new();
        b.push(&rows.to_variant());
        dbg.send_message(crate::editor::MSG_ENTITIES, &b);
    }

    fn flush(&mut self, outgoing: Vec<netcore::Outgoing>) {
        if let Some(mut inner) = self.inner.clone() {
            let connected = inner
                .get_multiplayer_peer()
                .is_some_and(|p| p.get_connection_status() == ConnectionStatus::CONNECTED);
            if connected {
                let known = inner.get_peers();
                for o in outgoing {
                    let to = o.peer as i32;
                    if !known.as_slice().contains(&to) {
                        continue;
                    }
                    let mode = match o.reliability {
                        Reliability::Reliable => TransferMode::RELIABLE,
                        Reliability::Unreliable => TransferMode::UNRELIABLE,
                    };
                    let bytes = PackedByteArray::from(o.bytes.as_slice());
                    let ch = usize::from(o.channel).min(2);
                    self.stats.sent[ch] += o.bytes.len() as u64;
                    PACKETS_SENT.fetch_add(1, Ordering::Relaxed);
                    inner
                        .send_bytes_ex(&bytes)
                        .id(to)
                        .mode(mode)
                        .channel(i32::from(o.channel))
                        .done();
                }
            }
        }
    }

    fn roll_stats_window(&mut self) {
        let rate = tick_rate();
        let c = &mut self.stats;
        c.window_ticks += 1;
        if c.window_ticks >= rate {
            for i in 0..3 {
                c.sent_per_s[i] = c.sent[i] - c.window_start_sent[i];
                c.received_per_s[i] = c.received[i] - c.window_start_received[i];
            }
            c.window_start_sent = c.sent;
            c.window_start_received = c.received;
            c.rollbacks_per_s = c.rollbacks - c.window_start_rollbacks;
            c.window_start_rollbacks = c.rollbacks;
            if let Some(core) = self.core.as_ref() {
                let i = core.interp().timeline.stats;
                c.late_per_s = i.late_snapshots - c.window_start_late;
                c.window_start_late = i.late_snapshots;
                c.corrections_per_s = i.corrections - c.window_start_corrections;
                c.window_start_corrections = i.corrections;
            }
            if let Some(core) = self.core.as_ref() {
                let cs = core.call_stats();
                let now = [cs.sent, cs.received, cs.refused, cs.dropped];
                for (i, n) in now.iter().enumerate() {
                    c.calls_per_s[i] = n - c.window_start_calls[i];
                }
                c.window_start_calls = now;
            }
            if let Some(core) = self.core.as_ref() {
                let div = core.hash_stats().divergences;
                c.divergences_per_s = div - c.window_start_divergences;
                c.window_start_divergences = div;
                let sb = core.repl_stats().snapshot_bytes;
                c.snapshot_bytes_per_s = sb - c.window_start_snapshot_bytes;
                c.window_start_snapshot_bytes = sb;
                let k = core.keyed().stats;
                let kb = k.bytes_sent + k.bytes_received;
                c.keyed_bytes_per_s = kb - c.window_start_keyed;
                c.window_start_keyed = kb;
                let r = core.repl_stats();
                let now = [r.deferred_entities, r.deferred_fields];
                for (i, n) in now.iter().enumerate() {
                    c.deferred_per_s[i] = n - c.window_start_deferred[i];
                }
                c.window_start_deferred = now;
            }
            c.poll_ms_avg = c.poll_us_window as f64 / 1000.0 / f64::from(c.window_ticks);
            c.poll_us_window = 0;
            c.window_ticks = 0;
        }
    }

    /// WP14, server: scores interaction (physics.rs) and hands the changes to netcore, before this
    /// tick's snapshots are built.
    fn interaction_pass(&mut self) {
        let spent = self.interaction_pass_inner();
        let rate = tick_rate().max(1);
        let w = &mut self.interaction_window;
        w.0 += spent;
        w.1 += 1;
        if w.1 >= rate {
            w.2 = w.0 as f64 / f64::from(w.1) / 1000.0;
            w.0 = 0;
            w.1 = 0;
        }
    }

    /// The pass itself; returns the microseconds it took (0 when skipped).
    fn interaction_pass_inner(&mut self) -> u64 {
        let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
        let every = crate::physics::every_ticks();
        if every > 1 && !tick.is_multiple_of(every) {
            // A skipped tick: the last sets stay, re-asserted (an entity that came back into a peer's
            // scope gets a fresh per-peer record), with no engine calls.
            if let Some(core) = self.core.as_mut() {
                for (peer, set) in &self.interacting {
                    for id in set {
                        core.set_interacting(*peer, *id, true);
                    }
                }
            }
            return 0;
        }
        let t0 = now_us();
        // The server's own player (a host) receives no snapshots: nothing to score for it.
        let players: Vec<(u32, NetId)> = self
            .core
            .as_ref()
            .map(|c| c.players())
            .unwrap_or_default()
            .into_iter()
            .filter(|(peer, _)| *peer != netcore::SERVER)
            .collect();
        if players.is_empty() && self.interacting.is_empty() {
            self.interaction_us = 0;
            return 0;
        }
        self.interaction_passes += 1;
        let managers = self.managers();
        let sets = crate::physics::interacting_sets(&managers, &players);
        let mut next: HashMap<u32, BTreeSet<NetId>> = HashMap::new();
        for (peer, set) in sets {
            next.entry(peer).or_default().extend(set);
        }
        let prev = std::mem::take(&mut self.interacting);
        if let Some(core) = self.core.as_mut() {
            for (peer, old) in &prev {
                let new = next.get(peer);
                for id in old {
                    if new.is_none_or(|n| !n.contains(id)) {
                        core.set_interacting(*peer, *id, false);
                    }
                }
            }
            // Every tick, not only on change: an entity that left a peer's scope and came back starts
            // with a fresh per-peer record.
            for (peer, set) in &next {
                for id in set {
                    core.set_interacting(*peer, *id, true);
                }
            }
        }
        self.interacting = next;
        self.interaction_us = now_us().saturating_sub(t0);
        self.interaction_us
    }

    /// Review 2026-11-08: the budget and the hash sample follow `Net` / the project settings while
    /// this session's own value is unset, so a write to `Net.budget_bytes_per_s` or `Net.hash_sample`
    /// reaches a running session at its next tick (the cold poll interval and the input delay were
    /// already read every tick).
    fn refresh_live_settings(&mut self) {
        let budget = self.effective_budget();
        let sample = self.effective_hash_sample();
        if let Some(c) = self.core.as_mut() {
            if c.budget() != budget {
                c.set_budget(budget);
            }
            let on = c.hash_telemetry();
            c.set_hash_telemetry(on, sample);
        }
    }

    fn effective_hash_sample(&self) -> u32 {
        if self.hash_sample >= 0 {
            self.hash_sample.min(1024) as u32
        } else {
            crate::hash::sample()
        }
    }

    fn effective_budget(&self) -> u32 {
        if self.budget_bytes_per_s > 0 {
            self.budget_bytes_per_s as u32
        } else {
            profiles::budget_bytes_per_s()
        }
    }

    pub fn effective_cold_poll(&self) -> u32 {
        if self.cold_poll_ticks > 0 {
            self.cold_poll_ticks as u32
        } else {
            profiles::cold_poll_ticks()
        }
    }

    pub fn debug_perturb_property(&self) -> String {
        self.debug_perturb_property.to_string()
    }

    /// WP6: a sent call waits for the `CallReturn` of request `request` (or its timeout).
    pub fn register_call(&mut self, request: u32, call: Gd<NetCall>) {
        self.calls.insert(request, call);
    }

    /// WP6: resolves the calls netcore answered (or timed out).
    fn resolve_calls(&mut self) {
        let mut done = Vec::new();
        if let Some(core) = self.core.as_mut() {
            while let Some(r) = core.poll_call_result() {
                done.push(r);
            }
        }
        for r in done {
            if let Some(call) = self.calls.remove(&r.request) {
                NetCall::resolve(&call, r.result.map(|v| net_call::decode_return(&v)));
            }
        }
    }

    /// WP7: a `Net.request_owner` call waits for the reply about entity `id`.
    pub fn register_owner_request(&mut self, id: u32, call: Gd<NetCall>) {
        self.owner_requests.entry(id).or_default().push(call);
    }

    /// WP7: resolves the `Net.request_owner` calls waiting on `id` with `granted`.
    pub fn resolve_owner_request(&mut self, id: u32, granted: bool) {
        for call in self.owner_requests.remove(&id).unwrap_or_default() {
            NetCall::resolve(&call, Ok(granted.to_variant()));
        }
    }

    /// WP7: a running netcore session exists (the keyed state uses it; else the local map).
    pub fn has_core(&self) -> bool {
        self.core.is_some()
    }

    /// WP7: the encoded value at `path` of the keyed state.
    pub fn keyed_get(&self, path: &str) -> Option<Vec<u8>> {
        self.core.as_ref()?.keyed().get(path).cloned()
    }

    pub fn keyed_keys(&self, prefix: &str) -> Vec<String> {
        self.core
            .as_ref()
            .map(|c| c.keyed().keys(prefix))
            .unwrap_or_default()
    }

    /// WP7: runs a keyed-state operation on the core and returns the events it queued (the caller
    /// emits them).
    pub fn keyed_op(
        &mut self,
        f: impl FnOnce(&mut Session, u64) -> Result<(), String>,
    ) -> Result<Vec<KeyEvent>, String> {
        let now = now_us();
        let Some(core) = self.core.as_mut() else {
            return Err(String::from("no session"));
        };
        f(core, now)?;
        let mut events = Vec::new();
        while let Some(e) = core.poll_key_event() {
            events.push(e);
        }
        Ok(events)
    }

    /// Makes `state` this session's keyed state object (`Net` passes its `Net.state`).
    pub fn set_state_object(&mut self, mut state: Gd<NetKeyedState>) {
        let iid = self.to_gd().instance_id();
        state.bind_mut().attach(Some(iid));
        self.state = state;
    }

    /// WP7, per tick: the server re-evaluates the keyed state's view filters for every joined peer;
    /// then every side emits the keyed state's signals for what changed.
    fn keyed_step(&mut self) {
        let is_server = self.is_server() && self.is_connected();
        if is_server {
            let views = self.state.bind().views();
            if !views.is_empty() {
                let peers: Vec<u32> = self
                    .core
                    .as_ref()
                    .map(|c| c.joined_peers())
                    .unwrap_or_default();
                let mut decisions = Vec::new();
                {
                    let _guard = self.base_mut();
                    for (prefix, filter) in &views {
                        for p in &peers {
                            decisions.push((
                                prefix.clone(),
                                *p,
                                net_keyed::call_filter(filter, *p),
                            ));
                        }
                    }
                }
                let now = now_us();
                if let Some(core) = self.core.as_mut() {
                    for (prefix, p, vis) in decisions {
                        core.keyed_set_visible(&prefix, p, vis, now);
                    }
                }
            }
        }
        let mut events = Vec::new();
        if let Some(core) = self.core.as_mut() {
            while let Some(e) = core.poll_key_event() {
                events.push(e);
            }
        }
        if events.is_empty() {
            return;
        }
        let mut state = self.state.clone();
        let _guard = self.base_mut();
        state.bind_mut().emit_events(events);
    }

    pub fn debug_extra_method(&self) -> String {
        self.debug_extra_method.to_string()
    }

    /// The session's generation (a new one per `start()`).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn debug_part_table_salt(&self) -> i64 {
        self.debug_part_table_salt
    }

    /// True while in the session (the server from its first tick, a client from `Welcome`).
    pub fn is_connected(&self) -> bool {
        self.core.as_ref().is_some_and(|c| c.is_connected())
    }

    /// True once the session ended or was never started.
    pub fn is_closed(&self) -> bool {
        self.core.as_ref().is_none_or(|c| c.is_closed())
    }

    /// Runs `f` on the netcore session with the current time; None when there is none. Keep it short:
    /// never call into the engine from `f`.
    pub fn with_core<R>(&mut self, f: impl FnOnce(&mut Session, u64) -> R) -> Option<R> {
        let now = now_us();
        self.core.as_mut().map(|c| f(c, now))
    }

    /// The key of a manager root: a hash of its path relative to this session's branch root, equal on
    /// every peer when the levels match.
    pub fn manager_key(&self, root: &Gd<Node>) -> u64 {
        let rel = self
            .branch_root
            .as_ref()
            .filter(|b| b.is_instance_valid())
            .map(|b| b.get_path_to(root).to_string())
            .unwrap_or_else(|| root.get_path().to_string());
        netcore::entity::fnv1a64(rel.as_bytes())
    }

    /// WP13: this session's branch root while valid.
    pub(crate) fn branch(&self) -> Option<Gd<Node>> {
        self.branch_root
            .as_ref()
            .filter(|b| b.is_instance_valid())
            .cloned()
    }

    /// The managers serving this session's branch, with their keys.
    pub(crate) fn managers(&self) -> Vec<(u64, Gd<NetManager>)> {
        let (Some(tree), Some(api)) = (scene_tree(), self.api.as_ref()) else {
            return Vec::new();
        };
        let api_id = api.instance_id();
        tree.get_nodes_in_group(MANAGERS_GROUP)
            .iter_shared()
            .filter_map(|n| n.try_cast::<NetManager>().ok())
            .filter(|m| {
                m.is_inside_tree()
                    && m.get_multiplayer()
                        .is_some_and(|a| a.instance_id() == api_id)
            })
            .map(|m| {
                let root = m.bind().get_managed_root();
                (self.manager_key(&root), m)
            })
            .collect()
    }

    /// 2.2 between steps 1 and 6 for entities: routes netcore's entity events to the managers by key
    /// and lets every manager apply them and announce its own changes. User code runs inside (spawn
    /// callbacks, `_ready` of replicas), so the base guard is held and core access is re-entrant.
    fn sync_managers(&mut self) {
        let mut events: Vec<(u64, bool, EntityEvent)> = std::mem::take(&mut self.unrouted);
        if let Some(core) = self.core.as_mut() {
            let tick = core.current_tick();
            while let Some(e) = core.poll_entity_event() {
                events.push((tick, false, e));
            }
        }
        // WP13: the server's managers register their rooms; a client's `Travel` loads its level first,
        // so the events after it find the level's managers.
        let is_server = self.is_server();
        if is_server && self.is_connected() {
            self.register_rooms();
            // WP16: the levels of neighbour and added rooms nobody registered yet.
            self.load_wanted_levels();
        }
        // WP16: a `RoomLeft` frees its level after this tick's moves (see `free_left_levels`).
        let mut rest = Vec::with_capacity(events.len());
        for (since, reported, e) in events {
            match e {
                EntityEvent::Travel {
                    scene_path,
                    room,
                    origin,
                } => {
                    self.on_travel_event(&scene_path, &room, origin);
                }
                EntityEvent::RoomLeft { room } => {
                    self.on_room_left(&room);
                }
                e => rest.push((since, reported, e)),
            }
        }
        let events = rest;
        let managers = self.managers();
        let updates: Vec<StateUpdate> = self
            .core
            .as_mut()
            .map(|c| c.drain_state_updates())
            .unwrap_or_default();
        if managers.is_empty() && events.is_empty() {
            self.free_left_levels();
            return;
        }
        let mut routed: HashMap<u64, Vec<EntityEvent>> = HashMap::new();
        let mut moves: Vec<(NetId, u64, ParentRef)> = Vec::new();
        let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
        let rate = u64::from(tick_rate());
        let mut keep: Vec<(u64, bool, EntityEvent)> = Vec::new();
        let present = |k: u64| managers.iter().any(|(mk, _)| *mk == k);
        for (since, reported, e) in events {
            let key = e.manager();
            let waiting = |k: u64, keep: &Vec<(u64, bool, EntityEvent)>| {
                keep.iter().any(|(_, _, w)| w.manager() == k)
            };
            // WP13: a move between managers applies once both exist (the client loaded the level).
            if let Some((id, from, parent)) = crate::travel::is_cross_move(&e) {
                if present(from) && present(parent.manager) && !waiting(from, &keep) {
                    moves.push((id, from, parent));
                    continue;
                }
            }
            // WP13: a destroy (or a scope leave) for a level this client already freed frees nothing.
            if matches!(e, EntityEvent::Destroy { .. })
                && !is_server
                && !present(key)
                && !waiting(key, &keep)
            {
                continue;
            }
            if present(key) && !waiting(key, &keep) {
                routed.entry(key).or_default().push(e);
            } else {
                let mut reported = reported;
                if !reported && tick.saturating_sub(since) >= 5 * rate {
                    reported = true;
                    godot_error!(
                        "gdnet: an entity action for a NetManager this peer does not have has waited 5 s (manager key {key:016x}); the managers must sit at the same path in every peer's branch"
                    );
                }
                keep.push((since, reported, e));
            }
        }
        self.unrouted = keep;
        let mut states: HashMap<u64, Vec<StateUpdate>> = HashMap::new();
        if let Some(core) = self.core.as_ref() {
            for u in updates {
                if let Some(r) = core.entities().get(u.id) {
                    states.entry(r.parent.manager).or_default().push(u);
                }
            }
        }
        let me = self.to_gd();
        let _guard = self.base_mut();
        for (key, mut m) in managers.clone() {
            let evs = routed.remove(&key).unwrap_or_default();
            let sts = states.remove(&key).unwrap_or_default();
            m.bind_mut().sync(me.clone(), key, evs);
            if !sts.is_empty() {
                m.bind_mut().apply_states(sts);
            }
        }
        drop(_guard);
        // WP13: moves between managers, after the managers applied this tick's events.
        for (id, from, parent) in moves {
            self.apply_move(&managers, id, from, parent);
        }
        self.free_left_levels();
    }

    /// 2.2 steps 2-4 (WP4): who simulates and prediction tracking, then a rollback and its replay when
    /// netcore asks for one, then this tick's inputs (sampling on owners, received frames on the server,
    /// `_net_input()`), then `_net_tick` for the parts that define it. The parts' own
    /// `_physics_process` runs after this node, as the fresh tick.
    fn input_step(&mut self) {
        if !self.is_connected() {
            return;
        }
        let managers = self.managers();
        let ctx = {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            core.set_input_delay(tick::input_delay());
            core.set_adaptive_delay(tick::adaptive_delay());
            TickCtx {
                is_server: core.role() == netcore::Role::Server,
                local_id: core.local_id(),
                tick: core.predicted_tick(),
                delay0: core.input_delay() == 0,
                delta: 1.0 / f64::from(core.tick_rate()),
            }
        };
        if managers.is_empty() {
            return;
        }
        let me = self.to_gd();
        {
            let _guard = self.base_mut();
            for (_, m) in &managers {
                let mut m = m.clone();
                m.bind_mut().gate_and_track(&me);
            }
        }
        let rollback = self.core.as_mut().and_then(|c| c.take_rollback());
        // The lead grew: the predicted ticks it skipped are simulated now (a rollback replays them).
        let catch_up = match (self.last_ptick, &rollback) {
            (Some(last), None) if !ctx.is_server && ctx.tick > last + 1 => Some(last + 1),
            _ => None,
        };
        self.last_ptick = Some(ctx.tick);
        let replayed = {
            let _guard = self.base_mut();
            let replayed = rollback
                .as_ref()
                .map(|rb| replay::rollback(&me, &managers, rb, ctx.tick, ctx.delta));
            if let Some(from) = catch_up {
                replay::catch_up(&me, &managers, from, ctx.tick, ctx.delta);
            }
            for (_, m) in &managers {
                let mut m = m.clone();
                m.bind_mut().prepare_inputs(&me, &ctx);
            }
            for (_, m) in &managers {
                let mut m = m.clone();
                m.bind_mut().net_ticks(&ctx);
            }
            replayed
        };
        if let Some(n) = replayed {
            self.stats.rollbacks += 1;
            self.stats.replayed_ticks += n;
        }
    }

    fn apply_emulation(&mut self) {
        let (send, receive, seed) = settings_of(&self.emulation);
        if self.applied == Some((send, receive)) {
            return;
        }
        let Some(peer) = self.peer.clone() else {
            return;
        };
        match peer.try_cast::<NetPipePeer>() {
            Ok(mut pipe) => {
                let emu = self.emulation.clone();
                pipe.bind_mut().set_emulation(emu);
                if let Some(c) = self.core.as_mut() {
                    c.set_emulation(EmulSettings::default(), EmulSettings::default(), seed);
                }
            }
            Err(_) => {
                if let Some(c) = self.core.as_mut() {
                    c.set_emulation(send, receive, seed);
                }
            }
        }
        self.applied = Some((send, receive));
    }

    fn stop(&mut self) {
        if self.core.is_none() {
            return;
        }
        self.interacting.clear();
        for (_, call) in self.calls.drain() {
            NetCall::resolve(
                &call,
                Err(String::from(
                    "the session ended before the call was answered",
                )),
            );
        }
        for (_, calls) in self.owner_requests.drain() {
            for call in calls {
                NetCall::resolve(
                    &call,
                    Err(String::from(
                        "the session ended before the ownership request was answered",
                    )),
                );
            }
        }
        self.restore_owned_offsets();
        let Some(_core) = self.core.take() else {
            return;
        };
        let me = self.to_gd();
        if let Some(mut tree) = scene_tree() {
            let cb = Callable::from_object_method(&me, PHYSICS_FRAME_HOOK.1);
            if tree.is_connected(PHYSICS_FRAME_HOOK.0, &cb) {
                tree.disconnect(PHYSICS_FRAME_HOOK.0, &cb);
            }
        }
        if let Some(mut inner) = self.inner.take() {
            for (signal, method) in HOOKS {
                let cb = Callable::from_object_method(&me, method);
                if inner.is_connected(signal, &cb) {
                    inner.disconnect(signal, &cb);
                }
            }
            if let Some(mut tree) = scene_tree() {
                match self.restore.take() {
                    Some(Restore::Default { api, peer }) => {
                        tree.set_multiplayer(&api);
                        let peer =
                            peer.unwrap_or_else(|| OfflineMultiplayerPeer::new_gd().upcast());
                        inner.set_multiplayer_peer(&peer);
                    }
                    Some(Restore::Branch { path, previous }) => {
                        tree.set_multiplayer_ex(previous.as_ref())
                            .root_path(&path)
                            .done();
                    }
                    None => {}
                }
            }
        }
        if let Some(mut api) = self.api.take() {
            NetMultiplayerApi::unwrap_inner(&mut api);
        }
        if let Some(mut peer) = self.peer.take() {
            peer.close();
        }
        self.inbox.clear();
        self.applied = None;
        self.base_mut().set_physics_process(false);
        self.base_mut().set_process(false);
    }

    /// 2.2 step 7 (WP5, see interp.rs): advances the render timeline, writes the replicas' interpolated
    /// fields that changed, and shows the owned offsets (`property = simulated + offset`). A client's
    /// session only; anything else returns at once.
    fn render_frame(&mut self) {
        let (values, owned) = {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            if core.role() != netcore::Role::Client || !core.is_connected() {
                return;
            }
            let values = core.interp_frame(now_us());
            (values, core.owned_offset_keys())
        };
        if values.is_empty() && owned.is_empty() && !self.owned_active {
            return;
        }
        self.owned_active = !owned.is_empty();
        let (mut by_key, mut owned_by_key) = (HashMap::new(), HashMap::new());
        if let Some(core) = self.core.as_ref() {
            let key_of = |id: netcore::NetId| core.entities().get(id).map(|r| r.parent.manager);
            for (k, v) in values {
                if let Some(m) = key_of(k.0) {
                    by_key.entry(m).or_insert_with(Vec::new).push((k, v));
                }
            }
            for k in owned {
                if let Some(m) = key_of(k.0) {
                    owned_by_key.entry(m).or_insert_with(Vec::new).push(k);
                }
            }
        }
        let managers = self.managers();
        let me = self.to_gd();
        let _guard = self.base_mut();
        for (key, mut m) in managers {
            if let Some(vals) = by_key.remove(&key) {
                m.bind_mut().apply_interp(vals);
            }
            let keys = owned_by_key.remove(&key).unwrap_or_default();
            m.bind_mut().show_owned(&me, &keys);
        }
    }

    /// Writes the simulated values back where the last render frame showed owned offsets.
    fn restore_owned_offsets(&mut self) {
        if !self.owned_active {
            return;
        }
        let managers = self.managers();
        let _guard = self.base_mut();
        for (_, mut m) in managers {
            m.bind_mut().restore_owned();
        }
    }
}

fn scene_tree() -> Option<Gd<SceneTree>> {
    Engine::singleton()
        .get_main_loop()
        .and_then(|m| m.try_cast::<SceneTree>().ok())
}

/// The session's tick rate: the engine's physics ticks per second.
pub fn tick_rate() -> u32 {
    Engine::singleton().get_physics_ticks_per_second().max(1) as u32
}

fn now_us() -> u64 {
    Time::singleton().get_ticks_usec()
}
