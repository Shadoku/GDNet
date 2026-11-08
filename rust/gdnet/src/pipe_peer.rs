//! `NetPipePeer`: an in-memory `MultiplayerPeer` (docs/PLAN_NETCODE.md 4, "Headless two-peer testing").
//! A server end and any number of client ends share one hub; packets travel between them through the
//! netcore emulator, so one headless process can hold a server branch and client branches. It is also
//! the production `Net.offline()` peer: a server end with no clients (the host is a client of itself).
//!
//! Semantics follow `ENetMultiplayerPeer`: the server is id 1 and always connected; a client end is
//! `CONNECTING` until its first `poll()`, then `CONNECTED`, emitting `peer_connected(1)` (the server end
//! emits `peer_connected(<client id>)` on its next poll); a client's packets always go to the server
//! (client-to-client traffic is relayed by `SceneMultiplayer`, `is_server_relay_supported()` is true);
//! `close()` and `disconnect_peer()` emit `peer_disconnected` on the other side's next poll.
//!
//! Emulation: each end's `NetEmulation` applies its `send_*` values to what it sends and its
//! `receive_*` values to what it receives. Reliable packets are never lost or reordered but every
//! simulated loss costs a resend delay; at loss 1.0 nothing arrives (see netcore::emul). The emulator's
//! clock is the engine's physics frame counter, so delays are whole physics ticks and tests that count
//! physics frames are deterministic.
//!
//! Everything lives on the main thread: the hub is an `Rc<RefCell<_>>`, never shared with a thread.

use crate::emulation::{settings_of, NetEmulation};
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::{Engine, IMultiplayerPeerExtension, MultiplayerPeerExtension};
use godot::global::Error;
use godot::prelude::*;
use netcore::{EmulSettings, Emulator};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

const SERVER_ID: i32 = 1;
const MAX_PACKET: i32 = 1 << 24;

#[derive(Clone)]
struct Packet {
    from: i32,
    to: i32,
    channel: i32,
    mode: TransferMode,
    data: Vec<u8>,
}

impl Packet {
    fn ordered(&self) -> Option<u8> {
        (self.mode == TransferMode::RELIABLE).then_some(self.channel.clamp(0, 255) as u8)
    }
}

enum PeerEvent {
    Connected(i32),
    Disconnected(i32),
}

struct End {
    status: ConnectionStatus,
    /// Ids this end is linked to (the server: every client; a client: the server).
    links: Vec<i32>,
    inbox: VecDeque<Packet>,
    events: VecDeque<PeerEvent>,
    out_emul: Emulator<Packet>,
    in_emul: Emulator<Packet>,
}

impl End {
    fn new(status: ConnectionStatus, seed: u64) -> Self {
        Self {
            status,
            links: Vec::new(),
            inbox: VecDeque::new(),
            events: VecDeque::new(),
            out_emul: Emulator::new(EmulSettings::default(), seed),
            in_emul: Emulator::new(EmulSettings::default(), seed ^ 0x9e37_79b9_7f4a_7c15),
        }
    }
}

#[derive(Default)]
struct Hub {
    ends: BTreeMap<i32, End>,
    next_id: i32,
    seed: u64,
}

impl Hub {
    fn new_server() -> Self {
        let mut hub = Hub {
            ends: BTreeMap::new(),
            next_id: 2,
            seed: 1,
        };
        hub.ends
            .insert(SERVER_ID, End::new(ConnectionStatus::CONNECTED, 1));
        hub
    }

    fn add_client(&mut self) -> Option<i32> {
        if self
            .ends
            .get(&SERVER_ID)
            .is_none_or(|s| s.status != ConnectionStatus::CONNECTED)
        {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.seed += 1;
        let mut end = End::new(
            ConnectionStatus::CONNECTING,
            self.seed.wrapping_mul(0x2545_f491_4f6c_dd1d),
        );
        end.links.push(SERVER_ID);
        end.events.push_back(PeerEvent::Connected(SERVER_ID));
        self.ends.insert(id, end);
        let server = self.ends.get_mut(&SERVER_ID)?;
        server.links.push(id);
        server.events.push_back(PeerEvent::Connected(id));
        Some(id)
    }

    /// Breaks the link between `a` and `b`; each side gets `peer_disconnected` for the other.
    fn unlink(&mut self, a: i32, b: i32) {
        for (me, other) in [(a, b), (b, a)] {
            if let Some(end) = self.ends.get_mut(&me) {
                if let Some(i) = end.links.iter().position(|&x| x == other) {
                    end.links.remove(i);
                    end.events.push_back(PeerEvent::Disconnected(other));
                    if me != SERVER_ID {
                        end.status = ConnectionStatus::DISCONNECTED;
                    }
                }
            }
        }
    }

    fn close(&mut self, id: i32) {
        let links = self
            .ends
            .get(&id)
            .map(|e| e.links.clone())
            .unwrap_or_default();
        for other in links {
            self.unlink(id, other);
        }
        if let Some(end) = self.ends.get_mut(&id) {
            end.status = ConnectionStatus::DISCONNECTED;
            end.inbox.clear();
            end.events.clear();
        }
    }

    fn send(&mut self, packet: Packet, now: u64) {
        let Some(end) = self.ends.get_mut(&packet.from) else {
            return;
        };
        if end.status != ConnectionStatus::CONNECTED {
            return;
        }
        // A client's packets always go to the server; the server routes by target (0 all, -id all but id).
        let targets: Vec<i32> = if packet.from != SERVER_ID || packet.to == 0 {
            end.links.clone()
        } else if packet.to < 0 {
            end.links
                .iter()
                .copied()
                .filter(|&x| x != -packet.to)
                .collect()
        } else {
            end.links
                .iter()
                .copied()
                .filter(|&x| x == packet.to)
                .collect()
        };
        for to in targets {
            let p = Packet {
                to,
                ..packet.clone()
            };
            let ordered = p.ordered();
            end.out_emul.push(now, p, ordered);
        }
        self.pump(now);
    }

    /// Moves due packets: sender emulators -> receiver emulators -> inboxes.
    fn pump(&mut self, now: u64) {
        let mut moving: Vec<Packet> = Vec::new();
        for end in self.ends.values_mut() {
            moving.extend(end.out_emul.pop_ready(now));
        }
        for p in moving {
            if let Some(dest) = self.ends.get_mut(&p.to) {
                if dest.links.contains(&p.from) {
                    let ordered = p.ordered();
                    dest.in_emul.push(now, p, ordered);
                }
            }
        }
        for end in self.ends.values_mut() {
            let ready = end.in_emul.pop_ready(now);
            end.inbox.extend(ready);
        }
    }
}

/// The emulator's clock: the physics frame counter in microseconds.
fn now_us() -> u64 {
    let e = Engine::singleton();
    let tps = e.get_physics_ticks_per_second().max(1) as u64;
    e.get_physics_frames().saturating_mul(1_000_000) / tps
}

/// An in-memory multiplayer peer. Create ends with `NetPipePeer.create_server()` (or `new()`) and
/// `NetPipePeer.create_client(server)`, or both at once with `create_pair()`.
#[derive(GodotClass)]
#[class(init, tool, base = MultiplayerPeerExtension)]
pub struct NetPipePeer {
    base: Base<MultiplayerPeerExtension>,
    hub: Option<Rc<RefCell<Hub>>>,
    id: i32,
    target: i32,
    channel: i32,
    #[init(val = TransferMode::RELIABLE)]
    mode: TransferMode,
    refuse: bool,
    emulation: Option<Gd<NetEmulation>>,
    /// WP8: bytes and packets this end put and got (what the transport carried, headers included).
    counts: [i64; 4],
}

#[godot_api]
impl NetPipePeer {
    /// A new server end (id 1) with no clients yet.
    #[func]
    fn create_server() -> Gd<NetPipePeer> {
        let mut peer = NetPipePeer::new_gd();
        peer.bind_mut().ensure_hub();
        peer
    }

    /// A new client end connected to `server`'s hub; null when `server` is not an open server end or
    /// refuses new connections.
    #[func]
    fn create_client(server: Gd<NetPipePeer>) -> Option<Gd<NetPipePeer>> {
        let hub = {
            let mut s = server.clone();
            let mut b = s.bind_mut();
            b.ensure_hub();
            if b.id != SERVER_ID || b.refuse {
                godot_error!("NetPipePeer.create_client: the given end is not an open server end");
                return None;
            }
            b.hub.clone()?
        };
        let id = hub.borrow_mut().add_client()?;
        let mut peer = NetPipePeer::new_gd();
        {
            let mut b = peer.bind_mut();
            b.hub = Some(hub);
            b.id = id;
        }
        Some(peer)
    }

    /// `[server, client]`: a server end and one client end connected to it.
    #[func]
    fn create_pair() -> VarArray {
        let server = Self::create_server();
        let client = Self::create_client(server.clone());
        let mut arr = VarArray::new();
        arr.push(&server.to_variant());
        arr.push(&client.to_variant());
        arr
    }

    /// The emulation applied at this end (its `send_*` values to what it sends, `receive_*` to what it
    /// receives). Null for none.
    #[func]
    pub fn set_emulation(&mut self, emulation: Option<Gd<NetEmulation>>) {
        self.emulation = emulation;
        self.sync_emulation(true);
    }

    #[func]
    fn get_emulation(&self) -> Option<Gd<NetEmulation>> {
        self.emulation.clone()
    }

    /// WP8: bytes this end handed to the pipe (every packet as `SceneMultiplayer` put it, so raw
    /// packets carry its one-byte header).
    #[func]
    fn bytes_sent(&self) -> i64 {
        self.counts[0]
    }

    /// WP8: packets this end handed to the pipe.
    #[func]
    fn packets_sent(&self) -> i64 {
        self.counts[1]
    }

    /// WP8: bytes this end took out of the pipe.
    #[func]
    fn bytes_received(&self) -> i64 {
        self.counts[2]
    }

    /// WP8: packets this end took out of the pipe.
    #[func]
    fn packets_received(&self) -> i64 {
        self.counts[3]
    }

    /// Packets held by this end's emulators (sent and not yet released, or received and not yet due).
    #[func]
    fn in_flight(&self) -> i64 {
        self.with_end(|e| (e.out_emul.len() + e.in_emul.len()) as i64)
            .unwrap_or(0)
    }
}

impl NetPipePeer {
    fn ensure_hub(&mut self) {
        if self.hub.is_none() {
            self.hub = Some(Rc::new(RefCell::new(Hub::new_server())));
            self.id = SERVER_ID;
        }
    }

    fn sync_emulation(&mut self, reseed: bool) {
        self.ensure_hub();
        let (send, receive, seed) = settings_of(&self.emulation);
        let Some(hub) = &self.hub else { return };
        let mut hub = hub.borrow_mut();
        if let Some(end) = hub.ends.get_mut(&self.id) {
            if reseed {
                end.out_emul = rebuild(&end.out_emul, send, seed.wrapping_add(self.id as u64));
                end.in_emul = rebuild(
                    &end.in_emul,
                    receive,
                    seed.wrapping_add(self.id as u64).wrapping_mul(31),
                );
            } else {
                end.out_emul.set_settings(send);
                end.in_emul.set_settings(receive);
            }
        }
    }

    fn with_end<R>(&self, f: impl FnOnce(&End) -> R) -> Option<R> {
        let hub = self.hub.as_ref()?.borrow();
        hub.ends.get(&self.id).map(f)
    }
}

/// A fresh emulator with the new seed, unless packets are in flight (then only the settings change).
fn rebuild(old: &Emulator<Packet>, settings: EmulSettings, seed: u64) -> Emulator<Packet> {
    if old.is_empty() {
        Emulator::new(settings, seed)
    } else {
        let mut e = old.clone();
        e.set_settings(settings);
        e
    }
}

impl Drop for NetPipePeer {
    fn drop(&mut self) {
        if let Some(hub) = &self.hub {
            if let Ok(mut hub) = hub.try_borrow_mut() {
                hub.close(self.id);
                hub.ends.remove(&self.id);
            }
        }
    }
}

#[godot_api]
impl IMultiplayerPeerExtension for NetPipePeer {
    fn get_available_packet_count(&self) -> i32 {
        self.with_end(|e| e.inbox.len() as i32).unwrap_or(0)
    }

    fn get_max_packet_size(&self) -> i32 {
        MAX_PACKET
    }

    fn get_packet_script(&mut self) -> PackedByteArray {
        let Some(hub) = &self.hub else {
            return PackedByteArray::new();
        };
        let mut hub = hub.borrow_mut();
        let packet = hub.ends.get_mut(&self.id).and_then(|e| e.inbox.pop_front());
        match packet {
            Some(p) => {
                self.counts[2] += p.data.len() as i64;
                self.counts[3] += 1;
                PackedByteArray::from(p.data.as_slice())
            }
            None => PackedByteArray::new(),
        }
    }

    fn put_packet_script(&mut self, buffer: PackedByteArray) -> Error {
        self.ensure_hub();
        if buffer.len() > MAX_PACKET as usize {
            return Error::ERR_OUT_OF_MEMORY;
        }
        if self.get_connection_status() != ConnectionStatus::CONNECTED {
            return Error::ERR_UNCONFIGURED;
        }
        self.sync_emulation(false);
        self.counts[0] += buffer.len() as i64;
        self.counts[1] += 1;
        let packet = Packet {
            from: self.id,
            to: self.target,
            channel: self.channel,
            mode: self.mode,
            data: buffer.to_vec(),
        };
        if let Some(hub) = &self.hub {
            hub.borrow_mut().send(packet, now_us());
        }
        Error::OK
    }

    fn get_packet_channel(&self) -> i32 {
        self.with_end(|e| e.inbox.front().map_or(0, |p| p.channel))
            .unwrap_or(0)
    }

    fn get_packet_mode(&self) -> TransferMode {
        self.with_end(|e| e.inbox.front().map_or(TransferMode::RELIABLE, |p| p.mode))
            .unwrap_or(TransferMode::RELIABLE)
    }

    fn set_transfer_channel(&mut self, channel: i32) {
        self.channel = channel;
    }

    fn get_transfer_channel(&self) -> i32 {
        self.channel
    }

    fn set_transfer_mode(&mut self, mode: TransferMode) {
        self.mode = mode;
    }

    fn get_transfer_mode(&self) -> TransferMode {
        self.mode
    }

    fn set_target_peer(&mut self, peer: i32) {
        self.target = peer;
    }

    fn get_packet_peer(&self) -> i32 {
        self.with_end(|e| e.inbox.front().map_or(0, |p| p.from))
            .unwrap_or(0)
    }

    fn is_server(&self) -> bool {
        self.id == SERVER_ID || self.hub.is_none()
    }

    fn poll(&mut self) {
        self.ensure_hub();
        self.sync_emulation(false);
        let events: Vec<PeerEvent> = {
            let Some(hub) = &self.hub else { return };
            let mut hub = hub.borrow_mut();
            hub.pump(now_us());
            let Some(end) = hub.ends.get_mut(&self.id) else {
                return;
            };
            let events: Vec<PeerEvent> = end.events.drain(..).collect();
            for ev in &events {
                if let PeerEvent::Connected(_) = ev {
                    if end.status == ConnectionStatus::CONNECTING {
                        end.status = ConnectionStatus::CONNECTED;
                    }
                }
            }
            events
        };
        // Emit with the hub released and through base_mut(), so SceneMultiplayer may call back in.
        for ev in events {
            match ev {
                PeerEvent::Connected(id) => {
                    self.base_mut()
                        .emit_signal("peer_connected", &[i64::from(id).to_variant()]);
                }
                PeerEvent::Disconnected(id) => {
                    self.base_mut()
                        .emit_signal("peer_disconnected", &[i64::from(id).to_variant()]);
                }
            }
        }
    }

    fn close(&mut self) {
        if let Some(hub) = &self.hub {
            hub.borrow_mut().close(self.id);
        }
    }

    fn disconnect_peer(&mut self, peer: i32, _force: bool) {
        if let Some(hub) = &self.hub {
            hub.borrow_mut().unlink(self.id, peer);
        }
    }

    fn get_unique_id(&self) -> i32 {
        if self.hub.is_none() {
            SERVER_ID
        } else {
            self.id
        }
    }

    fn set_refuse_new_connections(&mut self, enable: bool) {
        self.refuse = enable;
    }

    fn is_refusing_new_connections(&self) -> bool {
        self.refuse
    }

    fn is_server_relay_supported(&self) -> bool {
        true
    }

    fn get_connection_status(&self) -> ConnectionStatus {
        if self.hub.is_none() {
            // A bare `NetPipePeer.new()` is a server end that has not been touched yet.
            return ConnectionStatus::CONNECTED;
        }
        self.with_end(|e| e.status)
            .unwrap_or(ConnectionStatus::DISCONNECTED)
    }
}
