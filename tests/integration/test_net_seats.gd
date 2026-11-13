extends TestCase
## WP17 (docs/PLAN_NETCODE.md 12.3, gate G12): seats, dropping and rejoining, the keyed rule `"seat"`,
## and session state. A client "drops" when `NetFixture.drop_client` frees its branch (its process
## ended) and "rejoins" through `rejoin_client`: a new branch, session and pipe end with the same seat
## key, so a new peer id. Headless over NetPipePeer, in both host modes where it applies (G2).

const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const CARD := preload("res://tests/fixtures/net/card.tscn")
const HAND_CARD := preload("res://tests/fixtures/net/hand_card.tscn")
const COIN := preload("res://tests/fixtures/levels/coin.tscn")
const VAULT := "res://tests/fixtures/levels/vault.tscn"
const LOBBY := "res://tests/fixtures/rooms/interior_b.tscn"
const SPACING := "gdnet/levels/spacing"
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]

var net: NetFixture
var _project_spacing: Variant = null


func before_each() -> void:
	_project_spacing = ProjectSettings.get_setting(SPACING, null)
	ProjectSettings.set_setting(SPACING, null)


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	ProjectSettings.set_setting(SPACING, _project_spacing)
	Net.rejoin_timeout_s = -2.0
	Net.key = ""
	if net != null:
		net.teardown()
		net = null
	if Net.is_online():
		Net.leave()


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float, loss: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.send_loss = loss
	e.receive_loss = loss
	e.seed = 17
	return e


## True when `branch` holds a spawned copy at `path`.
func _present(branch: Node, path: String) -> bool:
	return branch != null and branch.has_node(path) and Net.net_id(branch.get_node(path)) > 0


## Records a session's seat signals as ["dropped", seat], ["rejoined", seat, peer], ["released", seat].
func _seat_log(s: NetSession) -> Array:
	var log := []
	s.peer_dropped.connect(func(seat: int) -> void: log.append(["dropped", seat]))
	s.peer_rejoined.connect(func(seat: int, peer: int) -> void: log.append(["rejoined", seat, peer]))
	s.seat_released.connect(func(seat: int) -> void: log.append(["released", seat]))
	return log


## Counts `entity_despawning` per node name on a session.
func _despawns(s: NetSession) -> Dictionary:
	var counts := {}
	s.entity_despawning.connect(func(n: Node) -> void: counts[String(n.name)] = int(counts.get(String(n.name), 0)) + 1)
	return counts


func test_seats_number_the_players_and_every_peer_knows_them() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var first := 1 if f.host_mode == NetFixture.HostMode.DEDICATED else 2
		var sa := f.session(a).my_seat()
		var sb := f.session(b).my_seat()
		assert_eq([sa, sb], [first, first + 1], "seats in join order after the host's")
		assert_eq(f.server_session.my_seat(), 0 if f.host_mode == NetFixture.HostMode.DEDICATED else 1)
		assert_eq(f.server_session.seat(f.peer_id(a)), sa)
		assert_eq(f.server_session.peer_of(sb), f.peer_id(b))
		assert_true(await f.wait_until(func() -> bool: return f.session(a).seat(f.peer_id(b)) == sb), "A learns B's seat")
		assert_eq(f.session(b).peer_of(sa), f.peer_id(a))
		assert_eq(f.server_session.stats()["seats"], 2 if f.host_mode == NetFixture.HostMode.DEDICATED else 3)
		assert_eq(f.server_session.stats()["seats_away"], 0))
	assert_true(ok)


func test_a_second_connection_with_a_connected_key_is_refused() -> void:
	net = NetFixture.new(self, 2, NetFixture.HostMode.DEDICATED)
	net.client_sessions[0].key = "alice"
	net.client_sessions[1].key = "alice"
	net.start_server()
	net.client_sessions[0].start(net.peer(net.clients[0]), net.clients[0].get_path())
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].my_seat() == 1))
	net.client_sessions[1].start(net.peer(net.clients[1]), net.clients[1].get_path())
	assert_true(await net.wait_until(func() -> bool: return net.disconnect_reasons.has("ClientB")), "refused")
	assert_true(String(net.disconnect_reasons["ClientB"]).contains("already in use"), String(net.disconnect_reasons["ClientB"]))
	assert_true(net.client_sessions[0].is_online(), "the first connection keeps its seat")


## Gate G12: a client that owns a predicted player, a crate, a card with an `audience: owner` face and a
## `visible_to = owner` hand, and has an added room, drops under emulated loss and rejoins inside the
## window: it holds the same entities, room and private state, nothing it owned was destroyed, the
## other client saw `peer_dropped` then `peer_rejoined` and kept every entity, the dropped player stopped
## while away, and the rejoined client controls and predicts its player again.
func test_g12_a_dropped_client_rejoins_with_its_entities_rooms_and_private_state() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		f.server_session.rejoin_timeout_s = 30.0
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		f.set_emulation(a, _link(30.0, 0.1))
		var aid := f.peer_id(a)
		var seat := f.server_session.seat(aid)
		var server_log := _seat_log(f.server_session)
		var b_log := _seat_log(f.session(b))
		var b_gone := _despawns(f.session(b))
		var p: Node2D = Net.spawn_for(aid, PLAYER, managers["Server"], "P")
		var crate: Node = Net.spawn_for(aid, CRATE, managers["Server"], "Crate")
		var card: Node = Net.spawn_for(aid, CARD, managers["Server"], "Card")
		card.face = 7
		Net.spawn_for(aid, HAND_CARD, managers["Server"], "Hand")
		f.server_session.add_room(aid, "arena")
		for path in ["World/P", "World/Crate", "World/Card", "World/Hand"]:
			assert_true(await f.wait_until(func() -> bool: return _present(a, path)), "A has " + path)
		assert_true(await f.wait_until(func() -> bool: return a.get_node("World/Card").face == 7))
		assert_true(await f.wait_until(func() -> bool: return _present(b, "World/P") and _present(b, "World/Card")))
		await _frames(30)
		# A walks right; the server's copy follows.
		Input.action_press("right")
		var x0 := p.position.x
		assert_true(await f.wait_until(func() -> bool: return p.position.x > x0 + 20.0), "the server's player moves")
		# A's process ends while "right" is still held.
		f.drop_client(0)
		assert_true(await f.wait_until(func() -> bool: return server_log.size() >= 1), "the server noticed the drop")
		assert_eq(server_log, [["dropped", seat]])
		assert_true(await f.wait_until(func() -> bool: return b_log.size() >= 1), "B heard of it")
		assert_eq(b_log, [["dropped", seat]])
		assert_eq(f.server_session.peer_of(seat), 0, "the seat is away")
		assert_eq(Net.get_owner_of(p), aid, "its entities stay with the absent seat")
		await _frames(10)
		var stopped := p.position
		await _frames(20)
		assert_almost_eq(p.position.x, stopped.x, 0.01, "the dropped player stops (neutral input)")
		Input.action_release("right")
		assert_true(is_instance_valid(crate) and is_instance_valid(card), "nothing it owned was destroyed")
		# A comes back under loss, with a new peer id.
		var t_rejoin := Engine.get_physics_frames()
		var a2 := f.rejoin_client(0)
		f.add_manager_to(a2)
		f.set_emulation(a2, _link(30.0, 0.1))
		assert_true(await f.wait_until(func() -> bool: return f.session(a2).is_online() and f.session(a2).my_seat() == seat, 10.0), "the same seat")
		var a2id := f.peer_id(a2)
		assert_ne(a2id, aid, "a new peer id")
		for path in ["World/P", "World/Crate", "World/Card", "World/Hand"]:
			assert_true(await f.wait_until(func() -> bool: return _present(a2, path), 10.0), "A has %s again" % path)
		assert_true(await f.wait_until(func() -> bool: return a2.get_node("World/Card").face == 7), "the owner-only value")
		print("rejoined and holding its four entities %d ticks after connecting (30 ms each way, 10 %% loss; mode %d)" % [Engine.get_physics_frames() - t_rejoin, f.host_mode])
		for n: Node in [p, crate, card]:
			assert_eq(Net.get_owner_of(n), a2id, "%s belongs to the new id" % n.name)
		assert_eq(Net.get_owner_of(a2.get_node("World/P")), a2id)
		assert_has(Array(f.server_session.rooms(a2id)), "arena", "its rooms moved")
		assert_true(await f.wait_until(func() -> bool: return b_log.size() >= 2), "B heard of the rejoin")
		assert_eq(b_log, [["dropped", seat], ["rejoined", seat, a2id]])
		assert_eq(server_log, [["dropped", seat], ["rejoined", seat, a2id]])
		assert_true(await f.wait_until(func() -> bool: return Net.get_owner_of(b.get_node("World/P")) == a2id), "B learns the new owner")
		assert_eq(b_gone, {}, "B never lost an entity")
		assert_false(b.has_node("World/Hand"), "the hand stays the owner's")
		assert_eq(b.get_node("World/Card").face, -1, "and so does the face")
		# It controls and predicts its player again.
		await _frames(60)
		var cp: Node2D = a2.get_node("World/P")
		var c0 := cp.position.x
		var s0 := p.position.x
		Input.action_press("right")
		await get_tree().physics_frame
		await get_tree().physics_frame
		assert_gt(cp.position.x, c0, "the owner moves at once (prediction)")
		assert_true(await f.wait_until(func() -> bool: return p.position.x > s0 + 10.0), "the server follows")
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool: return absf(cp.position.x - p.position.x) < 1.0, 5.0), "they converge (owner %s, server %s)" % [cp.position, p.position]))
	assert_true(ok)


func test_after_the_timeout_the_seat_is_released_and_the_entities_go_to_the_server() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		f.server_session.rejoin_timeout_s = 0.25
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var aid := f.peer_id(a)
		var seat := f.server_session.seat(aid)
		var server_log := _seat_log(f.server_session)
		var b_log := _seat_log(f.session(b))
		var crate: Node = Net.spawn_for(aid, CRATE, managers["Server"], "Crate")
		assert_true(await f.wait_until(func() -> bool: return _present(b, "World/Crate")))
		f.drop_client(0)
		assert_true(await f.wait_until(func() -> bool: return server_log.size() >= 2, 3.0), "dropped, then released")
		assert_eq(server_log, [["dropped", seat], ["released", seat]])
		assert_eq(Net.get_owner_of(crate), 1, "back to the server")
		assert_true(await f.wait_until(func() -> bool: return Net.get_owner_of(b.get_node("World/Crate")) == 1))
		assert_eq(b_log, [["dropped", seat], ["released", seat]])
		assert_eq(f.server_session.stats()["seats_away"], 0)
		# The key comes back to a fresh seat and nothing else.
		var a2 := f.rejoin_client(0)
		f.add_manager_to(a2)
		assert_true(await f.wait_until(func() -> bool: return f.session(a2).my_seat() > 0))
		assert_eq(Net.get_owner_of(crate), 1))
	assert_true(ok)


func test_a_zero_timeout_gives_the_entities_back_at_once_as_before() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		Net.rejoin_timeout_s = 0.0
		assert_eq(f.server_session.rejoin_timeout_s, 0.0, "the session follows Net's live value")
		var managers := f.add_managers()
		var aid := f.peer_id(f.clients[0])
		var server_log := _seat_log(f.server_session)
		var crate: Node = Net.spawn_for(aid, CRATE, managers["Server"], "Crate")
		await _frames(5)
		f.drop_client(0)
		assert_true(await f.wait_until(func() -> bool: return Net.get_owner_of(crate) == 1, 2.0), "the server owns it")
		assert_eq(server_log.size(), 1)
		assert_eq(server_log[0][0], "released", "no hold, no peer_dropped"))
	assert_true(ok)


func test_a_seat_path_written_before_the_drop_is_writable_after_the_rejoin() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		f.server_session.rejoin_timeout_s = -1.0
		f.server_session.state.allow("hand/", "seat")
		var a: Node = f.clients[0]
		var seat := f.session(a).my_seat()
		var path := "hand/%d" % seat
		await _frames(5)
		assert_true(f.session(a).state.set(path, 3), "the seat writes its path")
		allow_errors = true
		assert_false(f.session(a).state.set("hand/%d" % (seat + 1), 3), "another seat's path is refused locally")
		assert_true(await f.wait_until(func() -> bool: return f.server_session.state.get(path) == 3))
		f.drop_client(0)
		await _frames(5)
		var a2 := f.rejoin_client(0)
		assert_true(await f.wait_until(func() -> bool: return f.session(a2).my_seat() == seat))
		assert_eq(f.session(a2).state.get(path), 3, "it gets its value back")
		var rejected := []
		f.session(a2).state.write_rejected.connect(func(p: String) -> void: rejected.append(p))
		assert_true(f.session(a2).state.set(path, 4))
		assert_true(await f.wait_until(func() -> bool: return f.server_session.state.get(path) == 4), "the rejoined seat writes it")
		await _frames(5)
		assert_eq(rejected, []))
	assert_true(ok)


## A client whose player is in a level rejoins: its burst starts with the `Travel` to that level (the
## held seat kept the level loaded), and its focus node and `show_to` peer list moved to the new id.
func test_a_rejoined_client_gets_its_level_focus_and_peer_lists_back() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var s := f.server_session
		s.rejoin_timeout_s = -1.0
		s.empty_level_timeout = 0.2
		var lobby := s.load_level(LOBBY)
		var lm := lobby.get_node("NetManager")
		var aid := f.peer_id(f.clients[0])
		Net.spawn_for(aid, PLAYER, lm, "P")
		var shown: Node2D = CRATE.instantiate()
		shown.name = "Shown"
		lm.add_child(shown)
		Net.show_to(shown, PackedInt32Array([aid]))
		var cam := Node2D.new()
		cam.name = "Cam"
		f.server.add_child(cam)
		s.set_focus(aid, cam)
		assert_true(await f.wait_until(func() -> bool:
			return _present(f.clients[0], "interior_b/NetManager/P") and _present(f.clients[0], "interior_b/NetManager/Shown")), "A is in the lobby")
		f.drop_client(0)
		await _frames(30)
		assert_has(Array(s.levels()), LOBBY, "the held seat keeps its level loaded past the empty timeout")
		var a2 := f.rejoin_client(0)
		assert_true(await f.wait_until(func() -> bool:
			return _present(a2, "interior_b/NetManager/P") and _present(a2, "interior_b/NetManager/Shown")), "back in the lobby, with what was shown to it")
		var a2id := f.peer_id(a2)
		assert_eq(s.get_focus(a2id), cam, "the focus moved")
		assert_null(s.get_focus(aid))
		assert_eq(shown.get_meta("_net_visible_to"), PackedInt32Array([a2id]), "the show_to list names the new id")
		assert_eq(Net.get_owner_of(a2.get_node("interior_b/NetManager/P")), a2id))
	assert_true(ok)


## A run saved on one server is hosted again: the keyed state and the seats come back, a client with
## its old key gets its seat, and the level a client-owned coin was in gives the coin back to that seat.
func test_a_saved_session_hosted_again_has_its_keyed_state_seats_and_levels() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = NetFixture.new(self, 2, mode)
		net.client_sessions[0].key = "alice"
		net.client_sessions[1].key = "bob"
		net.start()
		assert_true(await net.wait_connected())
		var s := net.server_session
		s.empty_level_timeout = 1000.0
		var bid := net.peer_id(net.clients[1])
		var bob_seat := s.seat(bid)
		s.state.allow("hand/", "seat")
		s.state.set("score/total", 42)
		s.state.set("hand/%d" % bob_seat, ["ace", "king"])
		var lobby := s.load_level(LOBBY)
		Net.spawn_for(bid, PLAYER, lobby.get_node("NetManager"), "PB")
		var vault := s.load_level(VAULT)
		var coin: Node = Net.spawn_for(bid, COIN, vault.get_node("NetManager"), "Mine")
		coin.value = 11
		assert_true(await net.wait_until(func() -> bool: return Net.net_id(coin) != 0))
		await _frames(3)
		var saved := s.save_state()
		assert_gt(saved.size(), 6)
		assert_eq(saved.slice(0, 4).get_string_from_ascii(), "GDSS")
		print("session state: %d bytes" % saved.size())
		net.teardown()
		net = null
		await _frames(2)
		# Hosted again: bob joins first this time, alice not at all.
		net = NetFixture.new(self, 1, mode)
		net.client_sessions[0].key = "bob"
		net.start_server(saved)
		var s2 := net.server_session
		s2.empty_level_timeout = 1000.0
		assert_eq(s2.state.get("score/total"), 42, "the keyed state")
		assert_eq(s2.stats()["seats_away"], 2, "alice's and bob's seats are reserved")
		net.start_clients()
		assert_true(await net.wait_connected())
		var c := net.clients[0]
		assert_eq(net.session(c).my_seat(), bob_seat, "bob's old seat")
		assert_true(await net.wait_until(func() -> bool: return net.session(c).state.get("hand/%d" % bob_seat) == ["ace", "king"]))
		assert_true(net.session(c).state.set("hand/%d" % bob_seat, ["queen"]), "the seat rule came back")
		var vault2 := s2.load_level(VAULT)
		var mine := vault2.get_node_or_null("NetManager/Mine")
		assert_not_null(mine, "the level came back from the state")
		assert_eq(mine.value, 11)
		assert_true(await net.wait_until(func() -> bool: return Net.get_owner_of(mine) == net.peer_id(c)), "the coin went back to bob's seat")
		net.teardown()
		net = null


func test_offline_starts_from_a_saved_state() -> void:
	assert_eq(Net.offline(), OK)
	Net.state.set("run/floor", 3)
	assert_eq(Net.my_seat(), 1, "offline the host is seat 1")
	var saved := Net.save_state()
	Net.leave()
	assert_eq(Net.offline(saved), OK)
	assert_eq(Net.state.get("run/floor"), 3, "the keyed state came back")
	assert_eq(Net.my_seat(), 1)
	Net.leave()
	allow_errors = true
	assert_ne(Net.offline(PackedByteArray([1, 2, 3, 4, 5, 6, 7])), OK, "a state that is not one is refused")
	assert_false(Net.is_online())
