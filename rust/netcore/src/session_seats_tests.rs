//! WP17 session tests: seats, dropping and rejoining, the `"seat"` keyed rule, session state
//! (docs/PLAN_NETCODE.md 12.3). A server and any number of clients with an instant transport; a client
//! that drops is removed, and a rejoin is a new client session with a new peer id and the old key.

use super::*;
use crate::interest::{Spec, Visible};
use crate::keyed::{KeyEvent, Who};
use crate::props::{Audience, Quant, TypeTag};
use crate::savestate::{SessionState, SessionStateError};
use crate::seats::{Hold, SeatInfo};
use std::collections::BTreeMap;

const TICK_US: u64 = 16_667;
const SCENE: &str = "res://tests/fixtures/net/player.tscn";

fn props() -> ScenePropsTable {
    let mut face = PropDesc::new("face", TypeTag::Int);
    face.audience = Audience::Owner;
    ScenePropsTable::new(std::vec![std::vec![
        PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
        face,
    ]])
}

fn values(x: f32, face: i64) -> Vec<Vec<Value>> {
    std::vec![std::vec![Value::Vector2([x, 0.0]), Value::Int(face)]]
}

fn root() -> ParentRef {
    ParentRef {
        manager: 1,
        entity: crate::entity::MANAGER_ROOT,
        part: 0,
    }
}

struct Lobby {
    server: Session,
    clients: BTreeMap<PeerId, Session>,
    now: u64,
    server_events: Vec<Event>,
    events: BTreeMap<PeerId, Vec<Event>>,
}

impl Lobby {
    /// A server; `host_key` gives the server a seat of its own (a listen server).
    fn new(host_key: Option<&str>) -> Self {
        Self::from_server(Session::server(SessionConfig::new(60)), host_key)
    }

    fn from_server(mut server: Session, host_key: Option<&str>) -> Self {
        server.register_scene(SCENE, 42);
        server.register_props(SCENE, props(), std::vec![".".into()]);
        if let Some(k) = host_key {
            server.take_server_seat(k);
        }
        let mut l = Self {
            server,
            clients: BTreeMap::new(),
            now: 1_000_000,
            server_events: Vec::new(),
            events: BTreeMap::new(),
        };
        l.steps(2);
        l
    }

    fn connect(&mut self, id: PeerId, key: &str) {
        let mut c = Session::client(SessionConfig::new(60));
        c.set_key(key);
        c.register_scene(SCENE, 42);
        c.register_props(SCENE, props(), std::vec![".".into()]);
        self.server.peer_connected(id, self.now);
        c.peer_connected(SERVER, self.now);
        self.clients.insert(id, c);
        self.events.insert(id, Vec::new());
        self.steps(3);
    }

    /// The transport loses `id` on both ends.
    fn drop_client(&mut self, id: PeerId) {
        self.clients.remove(&id);
        self.server.peer_disconnected(id, self.now);
        self.steps(1);
    }

    fn c(&mut self, id: PeerId) -> &mut Session {
        self.clients.get_mut(&id).expect("client")
    }

    fn step(&mut self) {
        self.now += TICK_US;
        let now = self.now;
        self.server.tick(now);
        self.server.send_snapshots(now);
        for c in self.clients.values_mut() {
            c.tick(now);
            c.send_snapshots(now);
        }
        for (id, c) in self.clients.iter_mut() {
            for o in c.outgoing() {
                self.server.receive(*id, &o.bytes, now);
            }
        }
        for o in self.server.outgoing() {
            if let Some(c) = self.clients.get_mut(&o.peer) {
                c.receive(SERVER, &o.bytes, now);
            }
        }
        while let Some(e) = self.server.poll_event() {
            self.server_events.push(e);
        }
        for (id, c) in self.clients.iter_mut() {
            while let Some(e) = c.poll_event() {
                self.events.entry(*id).or_default().push(e);
            }
        }
    }

    fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    fn spawn(&mut self, owner: PeerId, name: &str, spec: Spawn) -> NetId {
        let _ = name;
        let id = self.server.spawn(SCENE, spec, self.now).unwrap();
        let _ = owner;
        self.steps(2);
        id
    }

    fn owner_on(s: &Session, id: NetId) -> PeerId {
        s.entities().get(id).map_or(0, |r| r.owner)
    }
}

fn seat_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::SeatDropped(_) | Event::SeatRejoined { .. } | Event::SeatReleased(_)
            )
        })
        .cloned()
        .collect()
}

fn destroyed(events: &[EntityEvent]) -> Vec<NetId> {
    events
        .iter()
        .filter_map(|e| match e {
            EntityEvent::Destroy { record, .. } => Some(record.id),
            _ => None,
        })
        .collect()
}

fn drain(s: &mut Session) -> Vec<EntityEvent> {
    core::iter::from_fn(|| s.poll_entity_event()).collect()
}

#[test]
fn seats_follow_keys_and_every_peer_learns_them() {
    let mut l = Lobby::new(Some("host"));
    assert_eq!(l.server.my_seat(), Some(1), "the host's player is seat 1");
    l.connect(2, "alice");
    l.connect(3, "bob");
    assert_eq!(l.server.seat(2), Some(2));
    assert_eq!(l.server.seat(3), Some(3));
    assert_eq!(l.server.peer_of(3), 3);
    assert_eq!(
        l.c(2).my_seat(),
        Some(2),
        "a client learns its own seat in Welcome"
    );
    // ...and the others' (bob joined after alice: an announcement).
    assert_eq!(l.c(2).seat(3), Some(3));
    assert_eq!(l.c(2).peer_of(1), SERVER);
    assert_eq!(l.c(3).seat(2), Some(2));
    assert_eq!(
        l.c(3).seat_table(),
        std::vec![
            SeatInfo {
                seat: 1,
                peer: 1,
                away: false
            },
            SeatInfo {
                seat: 2,
                peer: 2,
                away: false
            },
            SeatInfo {
                seat: 3,
                peer: 3,
                away: false
            },
        ]
    );
    // A dedicated server holds no seat: the first client is seat 1.
    let mut d = Lobby::new(None);
    d.connect(5, "carol");
    assert_eq!(d.server.my_seat(), None);
    assert_eq!(d.c(5).my_seat(), Some(1));
}

#[test]
fn a_second_connection_with_a_connected_key_is_refused() {
    let mut l = Lobby::new(None);
    l.connect(2, "alice");
    l.connect(3, "alice");
    assert!(l.c(3).is_closed());
    let reason = l.events[&3]
        .iter()
        .find_map(|e| match e {
            Event::Disconnected(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert!(
        reason.contains("seat 1") && reason.contains("already"),
        "a clear reason: {reason}"
    );
    assert_eq!(
        l.server.peer_of(1),
        2,
        "the first connection keeps the seat"
    );
    assert!(l.c(2).is_connected());
}

#[test]
fn a_rejoin_takes_back_entities_player_rooms_visibility_views_and_owner_values() {
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Session);
    l.connect(2, "alice");
    l.connect(3, "bob");
    let player = l.spawn(
        2,
        "P",
        Spawn::new(root(), "P", 2, 1, std::vec![])
            .with_values(values(5.0, 7))
            .as_player(PlayerMark::IfNone),
    );
    let hand = l.spawn(
        2,
        "Hand",
        Spawn::new(root(), "Hand", 2, 1, std::vec![])
            .with_values(values(0.0, 9))
            .with_interest(Spec {
                visible: Visible::Owner,
                ..Spec::default()
            }),
    );
    let shown = l.spawn(
        1,
        "Shown",
        Spawn::new(root(), "Shown", SERVER, 1, std::vec![])
            .with_values(values(1.0, 0))
            .with_interest(Spec {
                visible: Visible::Peers([2].into_iter().collect()),
                ..Spec::default()
            }),
    );
    l.server.add_room(2, "cave", l.now);
    l.server.set_room(2, Some("hall"), l.now);
    l.server.keyed_view("secret/", &[(2, true)], l.now);
    l.server.keyed_set("secret/a", std::vec![1], l.now).unwrap();
    l.steps(2);
    assert!(l.c(2).entities().contains(hand) && l.c(2).entities().contains(shown));
    assert!(
        !l.c(3).entities().contains(hand),
        "visible to its owner only"
    );
    drain(&mut l.server);
    drain(l.c(3));
    l.server_events.clear();
    l.events.insert(3, Vec::new());

    l.drop_client(2);
    l.steps(5);
    assert_eq!(
        seat_events(&l.server_events),
        std::vec![Event::SeatDropped(1)]
    );
    assert_eq!(seat_events(&l.events[&3]), std::vec![Event::SeatDropped(1)]);
    assert_eq!(l.server.peer_of(1), 0, "away");
    assert_eq!(l.server.seat(2), Some(1), "the old id still names the seat");
    assert_eq!(l.c(3).peer_of(1), 0);
    assert_eq!(l.c(3).seat(2), Some(1));
    assert_eq!(
        Lobby::owner_on(&l.server, player),
        2,
        "owned by the absent seat"
    );
    assert_eq!(Lobby::owner_on(l.c(3), player), 2);
    assert!(
        drain(l.c(3)).is_empty(),
        "nothing changed for the other client"
    );
    assert_eq!(l.server.seat_stats(), (2, 1), "two seats, one away");

    l.connect(4, "alice");
    l.steps(3);
    assert_eq!(
        seat_events(&l.server_events),
        std::vec![
            Event::SeatDropped(1),
            Event::SeatRejoined {
                seat: 1,
                peer: 4,
                old: 2
            }
        ]
    );
    assert_eq!(
        seat_events(&l.events[&3]),
        std::vec![
            Event::SeatDropped(1),
            Event::SeatRejoined {
                seat: 1,
                peer: 4,
                old: 2
            }
        ]
    );
    assert_eq!(l.c(4).my_seat(), Some(1));
    for id in [player, hand] {
        assert_eq!(Lobby::owner_on(&l.server, id), 4);
        assert_eq!(
            Lobby::owner_on(l.c(4), id),
            4,
            "the burst names the new owner"
        );
    }
    assert_eq!(
        Lobby::owner_on(l.c(3), player),
        4,
        "SetOwner reached the other client"
    );
    let ev3 = drain(l.c(3));
    assert!(destroyed(&ev3).is_empty(), "the other client never lost it");
    assert!(
        l.c(4).entities().contains(shown),
        "visible_to lists moved to the new id"
    );
    assert!(!l.c(3).entities().contains(hand));
    assert_eq!(
        l.server.players(),
        std::vec![(4, player)],
        "the player mark moved"
    );
    assert_eq!(l.server.rooms_added(4), std::vec![String::from("cave")]);
    assert_eq!(l.server.room_override(4).as_deref(), Some("hall"));
    assert_eq!(
        l.c(4).keyed().get("secret/a"),
        Some(&std::vec![1]),
        "view decisions moved"
    );
    // The owner-only value reached the new owner in its burst.
    let created: Vec<(NetId, Vec<u8>)> = drain(l.c(4))
        .into_iter()
        .filter_map(|e| match e {
            EntityEvent::Create { create, .. } => Some((create.id, create.initial_state)),
            _ => None,
        })
        .collect();
    let scene = crate::entity::scene_hash(SCENE);
    let (_, bytes) = created.iter().find(|(id, _)| *id == hand).unwrap();
    let decoded = l.c(4).decode_state(scene, bytes).unwrap();
    assert!(decoded.contains(&(0, 1, Value::Int(9))), "{decoded:?}");
}

#[test]
fn the_rejoin_window_zero_releases_at_once_as_before() {
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Released);
    l.connect(2, "alice");
    l.connect(3, "bob");
    let p = l.spawn(2, "P", Spawn::new(root(), "P", 2, 1, std::vec![]));
    l.drop_client(2);
    l.steps(2);
    assert_eq!(Lobby::owner_on(&l.server, p), SERVER);
    assert_eq!(Lobby::owner_on(l.c(3), p), SERVER);
    assert_eq!(
        seat_events(&l.server_events),
        std::vec![Event::SeatReleased(1)]
    );
    assert_eq!(
        seat_events(&l.events[&3]),
        std::vec![Event::SeatReleased(1)]
    );
    assert_eq!(l.server.seat_stats(), (1, 0));
}

#[test]
fn a_positive_window_releases_after_its_ticks() {
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Ticks(30));
    l.connect(2, "alice");
    l.connect(3, "bob");
    let p = l.spawn(2, "P", Spawn::new(root(), "P", 2, 1, std::vec![]));
    l.drop_client(2);
    l.steps(25);
    assert_eq!(Lobby::owner_on(&l.server, p), 2, "held");
    l.steps(10);
    assert_eq!(Lobby::owner_on(&l.server, p), SERVER, "released");
    assert_eq!(Lobby::owner_on(l.c(3), p), SERVER);
    assert_eq!(
        seat_events(&l.server_events),
        std::vec![Event::SeatDropped(1), Event::SeatReleased(1)]
    );
    assert_eq!(
        seat_events(&l.events[&3]),
        std::vec![Event::SeatDropped(1), Event::SeatReleased(1)]
    );
    // The key now gets a fresh seat and nothing back.
    l.connect(4, "alice");
    assert_eq!(l.c(4).my_seat(), Some(1));
    assert_eq!(Lobby::owner_on(&l.server, p), SERVER);
}

#[test]
fn a_dropped_owner_is_simulated_with_neutral_input() {
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Session);
    l.connect(2, "alice");
    let p = l.spawn(2, "P", Spawn::new(root(), "P", 2, 1, std::vec![]));
    let held = InputFrame {
        pressed: std::vec![true],
        strength: std::vec![255],
        custom: std::vec![],
    };
    for _ in 0..20 {
        let t = l.c(2).predicted_tick();
        let _ = t;
        l.c(2).local_input(p, held.clone());
        l.step();
    }
    assert_eq!(
        l.server.server_input(p).map(|f| f.frame.clone()),
        Some(held.clone()),
        "the owner's frames arrive"
    );
    l.drop_client(2);
    let repeated = l.server.input_stats().repeated;
    l.steps(10);
    let f = l.server.server_input(p).cloned().unwrap();
    assert_eq!(
        f.frame,
        InputFrame::default(),
        "neutral, not the last frame"
    );
    assert!(!f.repeated);
    assert_eq!(
        l.server.input_stats().repeated,
        repeated,
        "an absent owner's frames are not counted as repeated"
    );
}

#[test]
fn the_seat_rule_lets_a_seat_write_its_path_across_a_rejoin() {
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Session);
    l.connect(2, "alice");
    l.connect(3, "bob");
    l.server.keyed_allow("hand/", Who::Seat, l.now);
    l.steps(2);
    assert!({
        let now = l.now;
        l.c(2).keyed_set("hand/1", std::vec![5], now)
    }
    .is_ok());
    assert!(
        {
            let now = l.now;
            l.c(2).keyed_set("hand/2", std::vec![5], now)
        }
        .is_err(),
        "another seat's path is refused locally"
    );
    l.steps(2);
    assert_eq!(l.server.keyed().get("hand/1"), Some(&std::vec![5]));
    l.drop_client(2);
    l.connect(4, "alice");
    l.steps(2);
    assert!({
        let now = l.now;
        l.c(4).keyed_set("hand/1", std::vec![6], now)
    }
    .is_ok());
    l.steps(2);
    assert_eq!(
        l.server.keyed().get("hand/1"),
        Some(&std::vec![6]),
        "the rejoined seat writes its old path"
    );
    let rejected = core::iter::from_fn(|| l.c(4).poll_key_event())
        .filter(|e| matches!(e, KeyEvent::Rejected(_)))
        .count();
    assert_eq!(rejected, 0);
}

#[test]
fn a_session_state_restores_keyed_state_and_reserves_seats() {
    let mut l = Lobby::new(Some("host"));
    l.connect(2, "alice");
    l.connect(3, "bob");
    l.server.keyed_allow("hand/", Who::Seat, l.now);
    l.server.keyed_set("score/2", std::vec![40], l.now).unwrap();
    l.steps(2);
    let st = l.server.session_state();
    assert_eq!(st.host_seat, Some(1));
    assert_eq!(st.seats.len(), 3);
    let bytes = st.encode();
    assert_eq!(&bytes[..4], b"GDSS");
    let back = SessionState::decode(&bytes).unwrap();
    assert_eq!(back, st);

    // Hosted again (another process: the host has a new random key, the clients new peer ids).
    let mut server = Session::server(SessionConfig::new(60));
    server.restore_state(&back);
    let mut m = Lobby::from_server(server, Some("another host key"));
    assert_eq!(
        m.server.my_seat(),
        Some(1),
        "the host takes the saved host seat"
    );
    assert_eq!(m.server.keyed().get("score/2"), Some(&std::vec![40]));
    assert_eq!(m.server.seat_stats(), (3, 2), "two seats reserved");
    m.connect(9, "bob");
    m.connect(8, "dave");
    assert_eq!(m.c(9).my_seat(), Some(3), "bob's old seat");
    assert_eq!(m.c(8).my_seat(), Some(4), "a new key takes a free number");
    assert_eq!(m.c(9).keyed().get("score/2"), Some(&std::vec![40]));
    assert!(
        {
            let now = m.now;
            m.c(9).keyed_set("hand/3", std::vec![1], now)
        }
        .is_ok(),
        "rules restored"
    );
    m.steps(400);
    assert_eq!(
        m.server.seat_stats(),
        (4, 1),
        "alice's seat is still reserved"
    );
}

#[test]
fn an_unknown_session_state_is_refused_clearly() {
    assert_eq!(
        SessionState::decode(b"nope"),
        Err(SessionStateError::NotSessionState)
    );
    let mut bytes = SessionState::default().encode();
    bytes[4] = 99;
    bytes[5] = 0;
    let err = SessionState::decode(&bytes).unwrap_err();
    assert_eq!(err, SessionStateError::UnknownVersion(99));
    assert!(err.to_string().contains("99"), "{err}");
    let mut bytes = SessionState::default().encode();
    bytes.truncate(6);
    bytes.push(0xff);
    assert!(matches!(
        SessionState::decode(&bytes),
        Err(SessionStateError::Corrupt(_))
    ));
}

/// WP19: a `handoff` body a dropped client held goes back to the server at once, at its last state,
/// while the client's own entities wait for its seat.
#[test]
fn a_handoff_holder_that_drops_gives_the_body_back_at_once_while_its_seat_is_held() {
    use crate::authority::Transfer;
    use crate::correct::PhysicsMode;
    let mut l = Lobby::new(None);
    l.server.set_rejoin_hold(Hold::Session);
    l.connect(2, "ka");
    let body = l
        .server
        .spawn(
            SCENE,
            Spawn::new(root(), "Crate", SERVER, 1, std::vec![])
                .with_values(values(1.0, 0))
                .with_authority(Transfer::Fixed, true)
                .with_physics(PhysicsMode::Handoff),
            l.now,
        )
        .unwrap();
    let own = l
        .server
        .spawn(
            SCENE,
            Spawn::new(root(), "Own", 2, 1, std::vec![]).with_values(values(2.0, 0)),
            l.now,
        )
        .unwrap();
    l.steps(2);
    let now = l.now;
    let c = l.clients.get_mut(&2).unwrap();
    assert!(c.claim(body, now));
    c.record_part(body, 0, &[(0, Value::Vector2([30.0, 0.0]))]);
    l.steps(4);
    let owner = |l: &Lobby, id: NetId| l.server.entities().get(id).map(|r| (r.owner, r.owner_seq));
    assert_eq!(owner(&l, body), Some((2, 1)));
    while l.server.poll_entity_event().is_some() {}
    l.server.drain_state_updates();
    l.server.peer_disconnected(2, l.now);
    assert_eq!(
        owner(&l, body),
        Some((SERVER, 2)),
        "back with the server at once"
    );
    assert_eq!(
        owner(&l, own).map(|o| o.0),
        Some(2),
        "its own entity waits for the seat"
    );
    assert_eq!(l.server.handoff_stats().returns, 1);
    let mut ev = Vec::new();
    while let Some(e) = l.server.poll_entity_event() {
        ev.push(e);
    }
    assert!(ev
        .iter()
        .any(|e| matches!(e, EntityEvent::SetOwner { id, owner: SERVER, .. } if *id == body)));
}
