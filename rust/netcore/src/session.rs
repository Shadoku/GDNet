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

use crate::clock::TickClock;
use crate::emul::{EmulSettings, Emulator};
use crate::peers::{PeerId, PeerTable, Stage, SERVER};
use crate::wire::{
    self, Hello, IdRange, Message, Ping, Pong, Protocol, Reject, Reliability, Welcome,
};
use std::collections::VecDeque;
use std::string::String;
use std::vec::Vec;

/// Entity ids each peer may allocate (placeholder until WP2): slot `n` owns `[n * ID_RANGE_LEN, ...)`.
pub const ID_RANGE_LEN: u32 = 1 << 16;

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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SessionConfig {
    /// Ticks per second (Godot's physics ticks per second).
    pub tick_rate: u32,
    /// Debug hook: any value other than 0 changes the protocol hash, forcing a mismatch.
    pub protocol_salt: u64,
    /// A `Time` ping goes to each joined peer every this many ticks (0 = a quarter second).
    pub ping_interval_ticks: u32,
}

impl SessionConfig {
    pub fn new(tick_rate: u32) -> Self {
        Self {
            tick_rate,
            protocol_salt: 0,
            ping_interval_ticks: 0,
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
}

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
            protocol: Protocol::new(rate, cfg.protocol_salt),
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

    /// This side's entity id range (placeholder until WP2).
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
    pub fn peer_disconnected(&mut self, peer: PeerId, _now_us: u64) {
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
}
