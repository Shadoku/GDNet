extends TestCase
## WP16 slice A (docs/PLAN_NETCODE.md 12.2 "Radius" and "Visibility", gates G9 and G0): radius interest
## with hysteresis and `set_focus`, `visible_to` and `Net.show_to`, the per-property `audience: owner`,
## `visibility_filter`, and no interest work without a radius. Headless over NetPipePeer, in both host
## modes (G2).

const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const MARKER := preload("res://tests/fixtures/net/radius_marker.tscn")
const CARD := preload("res://tests/fixtures/net/card.tscn")
const HAND_CARD := preload("res://tests/fixtures/net/hand_card.tscn")

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## True when `branch` holds a spawned copy at `path`.
func _present(branch: Node, path: String) -> bool:
	return branch.has_node(path) and Net.net_id(branch.get_node(path)) > 0


## Adds a new instance of `scene` named `node_name` at `pos` under `parent` (the server's manager).
func _add(parent: Node, scene: PackedScene, node_name: String, pos: Vector2) -> Node2D:
	var n: Node2D = scene.instantiate()
	n.name = node_name
	n.position = pos
	parent.add_child(n)
	return n


## Counts the client's `entity_spawned` / `entity_despawning` for nodes named `node_name`.
func _count(session: NetSession, node_name: String) -> Dictionary:
	var counts := {"spawned": 0, "despawning": 0}
	session.entity_spawned.connect(func(n: Node) -> void:
		if n.name == node_name:
			counts["spawned"] += 1)
	session.entity_despawning.connect(func(n: Node) -> void:
		if n.name == node_name:
			counts["despawning"] += 1)
	return counts


func test_an_entity_beyond_the_radius_is_absent_and_returns_with_current_state() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var server_world: Node = managers["Server"]
		var client: Node = f.clients[0]
		var cid := f.peer_id(client)
		var counts := _count(f.session(client), "Far")
		var player: Node2D = Net.spawn_for(cid, CRATE, server_world, "Player")
		var far := _add(server_world, MARKER, "Far", Vector2(300, 0))
		var conf: Node2D = CRATE.instantiate()
		conf.name = "Conf"
		conf.position = Vector2(0, 500)
		Net.configure(conf, {"radius": 50.0})
		server_world.add_child(conf)
		_add(server_world, CRATE, "Plain", Vector2(5000, 0))
		assert_true(await f.wait_until(func() -> bool:
			return _present(client, "World/Player") and _present(client, "World/Plain")), "the player and the plain crate arrived")
		await _frames(10)
		assert_false(client.has_node("World/Far"), "beyond NetEntity.radius: absent")
		assert_false(client.has_node("World/Conf"), "beyond configure()'s radius: absent")
		var by_peer: Dictionary = f.server_session.stats()["entities_in_scope_by_peer"]
		assert_eq(by_peer[cid], 2, "the server counts the client's scope")
		assert_eq(f.session(client).stats()["entities_in_scope"], 2, "the client holds two")
		# It moves within the radius (and changes): it arrives with the server's current state.
		far.position = Vector2(90, 0)
		far.modulate = Color(0, 1, 0)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Far")), "it entered at the radius")
		var copy: Node2D = client.get_node("World/Far")
		assert_true(await f.wait_until(func() -> bool:
			return copy.position.distance_to(Vector2(90, 0)) < 0.5 and copy.modulate.is_equal_approx(Color(0, 1, 0))), "with current state")
		# At the edge, between r and 1.2 r: no create/free churn.
		for x: float in [105.0, 115.0, 101.0, 119.0, 110.0, 118.0]:
			far.position = Vector2(x, 0)
			await _frames(3)
		assert_true(is_instance_valid(copy) and client.has_node("World/Far"), "still there at 1.18 r")
		assert_eq(counts, {"spawned": 1, "despawning": 0}, "no churn at the edge")
		# Beyond 1.2 r it leaves.
		far.position = Vector2(125, 0)
		assert_true(await f.wait_until(func() -> bool: return not client.has_node("World/Far")), "it left beyond 1.2 r")
		# Changed while away; the player walks to it and it returns with that state.
		far.modulate = Color(1, 0, 0)
		await _frames(10)
		player.position = Vector2(60, 0)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Far")), "it returned")
		copy = client.get_node("World/Far")
		assert_true(await f.wait_until(func() -> bool:
			return copy.modulate.is_equal_approx(Color(1, 0, 0)) and copy.position.distance_to(Vector2(125, 0)) < 0.5), "with the state it has now")
		assert_eq(counts, {"spawned": 2, "despawning": 1})
		assert_true(_present(client, "World/Player"), "the client's own player never left")
		assert_gt(f.server_session.stats()["interest_passes"], 0, "the radius pass ran")
		assert_eq(f.server_session.stats()["radius_entities"], 2))
	assert_true(ok)


func test_set_focus_and_the_managers_radius() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers("World", func(m: NetManager) -> void: m.radius = 100.0)
		var server_world: Node = managers["Server"]
		var client: Node = f.clients[0]
		var cid := f.peer_id(client)
		Net.spawn_for(cid, CRATE, server_world, "Player")
		_add(server_world, CRATE, "Far", Vector2(1000, 0))
		var unlimited: Node2D = MARKER.instantiate()
		unlimited.name = "Unlimited"
		unlimited.position = Vector2(-5000, 0)
		(unlimited.get_node("NetEntity") as NetEntity).radius = -1.0
		server_world.add_child(unlimited)
		assert_true(await f.wait_until(func() -> bool:
			return _present(client, "World/Player") and _present(client, "World/Unlimited")), "the player, and a radius of -1 opts out")
		await _frames(10)
		assert_false(client.has_node("World/Far"), "the manager's radius applies")
		# A camera far away is the client's focus now.
		var cam := Node2D.new()
		cam.position = Vector2(950, 0)
		f.server.add_child(cam)
		Net.set_focus(cid, cam)
		assert_eq(f.server_session.get_focus(cid), cam)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Far")), "near the focus: present")
		assert_true(_present(client, "World/Player"), "its own player stays (owned)")
		assert_eq(Array(f.server_session.in_radius(cid)), [Net.net_id(server_world.get_node("Far"))])
		# Back to the player.
		f.server_session.set_focus(cid, null)
		assert_true(await f.wait_until(func() -> bool: return not client.has_node("World/Far")), "away from the player: absent")
		cam.free())
	assert_true(ok)


func test_visible_to_owner_and_show_to() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var aid := f.peer_id(a)
		var bid := f.peer_id(b)
		var hand: Node = Net.spawn_for(aid, HAND_CARD, managers["Server"], "Hand")
		_add(managers["Server"], CRATE, "Table", Vector2.ZERO)
		assert_true(await f.wait_until(func() -> bool:
			return _present(a, "World/Hand") and _present(b, "World/Table")), "the owner has its card, everyone the table")
		await _frames(10)
		assert_false(b.has_node("World/Hand"), "visible_to = owner: never on the other client")
		# show_to adds the other client (the owner keeps it).
		Net.show_to(hand, PackedInt32Array([bid]))
		assert_true(await f.wait_until(func() -> bool: return _present(b, "World/Hand")), "shown to B")
		assert_true(_present(a, "World/Hand"))
		assert_eq((hand.get_node("NetEntity") as NetEntity).visible_to, 2, "show_to set visible_to = peers")
		assert_eq((hand.get_node("NetEntity") as NetEntity).visible_peers, PackedInt32Array([bid]))
		Net.show_to(hand, PackedInt32Array())
		assert_true(await f.wait_until(func() -> bool: return not b.has_node("World/Hand")), "hidden again")
		# A crate without a NetEntity, through configure(): only B.
		var secret := _add(managers["Server"], CRATE, "Secret", Vector2.ZERO)
		Net.configure(secret, {"visible_to": [bid]})
		assert_true(await f.wait_until(func() -> bool: return _present(b, "World/Secret")), "B has it")
		await _frames(10)
		assert_false(a.has_node("World/Secret"), "A never had it")
		# The NetEntity property at run time: everyone.
		(hand.get_node("NetEntity") as NetEntity).visible_to = 0
		assert_true(await f.wait_until(func() -> bool: return _present(b, "World/Hand")), "visible_to = all"))
	assert_true(ok)


func test_an_owner_only_property_keeps_the_scene_value_elsewhere() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var aid := f.peer_id(a)
		var bid := f.peer_id(b)
		var card: Node = Net.spawn_for(aid, CARD, managers["Server"], "Card")
		card.face = 7
		card.position = Vector2(3, 4)
		assert_true(await f.wait_until(func() -> bool:
			return _present(a, "World/Card") and _present(b, "World/Card")), "every client has the card")
		var ca: Node = a.get_node("World/Card")
		var cb: Node = b.get_node("World/Card")
		assert_true(await f.wait_until(func() -> bool:
			return ca.face == 7 and cb.position.distance_to(Vector2(3, 4)) < 0.5), "the owner sees the face, B the position")
		await _frames(10)
		assert_eq(cb.face, -1, "B keeps the scene's value")
		card.face = 8
		assert_true(await f.wait_until(func() -> bool: return ca.face == 8), "a change reaches the owner")
		await _frames(10)
		assert_eq(cb.face, -1, "and not B")
		# B takes the card: it learns the current face; A keeps the last one it saw.
		Net.set_owner(card, bid)
		assert_true(await f.wait_until(func() -> bool: return cb.face == 8), "the new owner receives the current value")
		card.face = 9
		assert_true(await f.wait_until(func() -> bool: return cb.face == 9))
		await _frames(10)
		assert_eq(ca.face, 8, "the old owner hears no more of it")
		assert_eq(f.session(b).stats()["divergences"], 0, "hash telemetry leaves the field out"))
	assert_true(ok)


func test_a_visibility_filter_decides_per_peer() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var aid := f.peer_id(a)
		var bid := f.peer_id(b)
		var allowed := [aid]
		var fog: Node2D = CRATE.instantiate()
		fog.name = "Fog"
		Net.set_visibility_filter(fog, func(peer: int) -> bool: return allowed.has(peer))
		managers["Server"].add_child(fog)
		_add(managers["Server"], CRATE, "Table", Vector2.ZERO)
		assert_true(await f.wait_until(func() -> bool:
			return _present(a, "World/Fog") and _present(b, "World/Table")), "the filter admits A")
		await _frames(10)
		assert_false(b.has_node("World/Fog"), "and not B")
		allowed[0] = bid
		assert_true(await f.wait_until(func() -> bool:
			return _present(b, "World/Fog") and not a.has_node("World/Fog")), "a new answer moves it within filter_every_ticks")
		assert_gt(f.server_session.stats()["filter_evaluations"], 1)
		# An invalid Callable removes the filter.
		Net.set_visibility_filter(fog, Callable())
		assert_true(await f.wait_until(func() -> bool: return _present(a, "World/Fog")), "no filter: everyone"))
	assert_true(ok)


func test_g0_no_radius_means_no_interest_pass() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		Net.spawn_for(f.peer_id(client), CRATE, managers["Server"], "Player")
		for i in 5:
			_add(managers["Server"], CRATE, "C%d" % i, Vector2(i * 1000, 0))
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/C4")))
		var before: Dictionary = f.server_session.stats()
		await _frames(30)
		var s: Dictionary = f.server_session.stats()
		assert_eq(s["interest_passes"], 0, "no radius: the pass never ran")
		assert_eq(s["filter_evaluations"], 0)
		assert_eq(s["radius_entities"], 0)
		assert_eq(s["interest_ms"], 0.0)
		assert_eq(s["scope_left"], 0, "nothing left a scope")
		assert_eq(s["scope_entered"], before["scope_entered"], "nothing entered by a scope change")
		# Nothing moves: no snapshot bytes, whatever interest would have done.
		assert_eq(s["snapshot_bytes"], before["snapshot_bytes"], "an idle scene sends no snapshots")
		assert_eq(f.session(client).stats()["entities_in_scope"], 6))
	assert_true(ok)


## Gate G9 (radius): 1,000 radius-limited markers over about 6,000 x 6,000 px and two clients whose
## players walk across them. Sampled every frame, a client holds no marker farther than 1.2 r from its
## player (the server's positions; the slack is the player's travel in three frames: the server's pass
## may run before the test moves the player in a frame, and the `Scope` leave is applied at the
## client's next poll). With `--verbose` it prints the interest pass's cost (the slice D benchmark).
func test_g9_no_entity_beyond_one_point_two_r() -> void:
	timeout_sec = 30.0
	const R := 300.0
	const SPEED := 8.0
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var server_world: Node = managers["Server"]
		var players: Array[Node2D] = []
		var dirs: Array[Vector2] = [Vector2(1, 0), Vector2(0.6, 0.8)]
		var starts: Array[Vector2] = [Vector2(200, 3000), Vector2(1500, 200)]
		for i in 2:
			var p: Node2D = Net.spawn_for(f.peer_id(f.clients[i]), CRATE, server_world, "Player%d" % i)
			p.position = starts[i]
			players.append(p)
		for i in 1000:
			var m: Node2D = MARKER.instantiate()
			m.name = "M%d" % i
			m.position = Vector2((i % 32) * 190.0, (i / 32) * 190.0)
			(m.get_node("NetEntity") as NetEntity).radius = R
			server_world.add_child(m)
		assert_true(await f.wait_until(func() -> bool:
			return _present(f.clients[0], "World/Player0") and _present(f.clients[1], "World/Player1")))
		var worst := 0.0
		var held := 0
		var ms: Array[float] = []
		var changes_before: int = f.server_session.stats()["radius_changes"]
		for frame in 150:
			for i in 2:
				players[i].position += dirs[i] * SPEED
			await get_tree().physics_frame
			ms.append(f.server_session.stats()["interest_ms"])
			for i in 2:
				var world: Node = f.clients[i].get_node("World")
				for n: Node in world.get_children():
					if not n.name.begins_with("M") or Net.net_id(n) <= 0:
						continue
					held += 1
					var d: float = (server_world.get_node(NodePath(n.name)) as Node2D).position.distance_to(players[i].position)
					worst = maxf(worst, d)
		assert_lt(worst, R * 1.2 + 3.0 * SPEED, "no marker beyond 1.2 r (worst %.1f px)" % worst)
		assert_gt(held, 1000, "the clients held markers near them (%d samples)" % held)
		var changes: int = f.server_session.stats()["radius_changes"] - changes_before
		assert_gt(changes, 50, "markers entered and left as the players walked")
		var total := 0.0
		var peak := 0.0
		for v in ms:
			total += v
			peak = maxf(peak, v)
		print("G9 radius, mode %d: 1000 markers, 2 clients, 150 ticks: interest pass avg %.3f ms, max %.3f ms; %d set changes; worst distance %.1f px (1.2 r = %.0f)" % [
			f.host_mode, total / ms.size(), peak, changes, worst, R * 1.2]))
	assert_true(ok)


## Gate G9 (audience): a value marked `audience: owner` is in no packet the other client receives
## (scanned at every bit offset by the pipe's test hook), and is in the owner's.
func test_g9_an_owner_only_value_is_in_no_packet_to_another_peer() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var b: Node = f.clients[1]
		var first := "G9-SECRET-7f3a9c41d2e8".to_utf8_buffer()
		var second := "G9-SECRET-b5c80e17aa64".to_utf8_buffer()
		for end: NetPipePeer in [f.peer(a), f.peer(b)]:
			end._debug_watch(first)
		var card: Node = Net.spawn_for(f.peer_id(a), CARD, managers["Server"], "Card")
		card.secret_name = "G9-SECRET-7f3a9c41d2e8"
		assert_true(await f.wait_until(func() -> bool:
			return _present(b, "World/Card") and a.has_node("World/Card") and a.get_node("World/Card").secret_name == "G9-SECRET-7f3a9c41d2e8"), "the owner learns it")
		assert_gt(f.peer(a)._debug_watch_hits(), 0, "the scan finds it in the owner's packets")
		# A change goes out as a reliable field.
		for end: NetPipePeer in [f.peer(a), f.peer(b)]:
			end._debug_watch(second)
		card.secret_name = "G9-SECRET-b5c80e17aa64"
		card.position = Vector2(40, 0)
		assert_true(await f.wait_until(func() -> bool: return a.get_node("World/Card").secret_name == "G9-SECRET-b5c80e17aa64"))
		await _frames(10)
		assert_gt(f.peer(a)._debug_watch_hits(), 0)
		assert_gt(f.peer(b)._debug_watch_scanned(), 0, "B received packets meanwhile")
		assert_eq(f.peer(b)._debug_watch_hits(), 0, "and none held the value")
		assert_eq(b.get_node("World/Card").secret_name, "", "B keeps the scene's value"))
	assert_true(ok)


## A client that joins after a `visibility_filter` was set gets the entity only once the filter
## admits it.
func test_a_late_joiner_gets_what_the_filter_admits() -> void:
	net = NetFixture.new(self, 2)
	net.start_server()
	var a: Node = net.clients[0]
	var b: Node = net.clients[1]
	net.client_sessions[0].start(net.peer(a), a.get_path())
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].is_online()))
	var managers := net.add_managers()
	var allowed := [net.peer_id(a)]
	var fog: Node2D = CRATE.instantiate()
	fog.name = "Fog"
	Net.set_visibility_filter(fog, func(peer: int) -> bool: return allowed.has(peer))
	managers["Server"].add_child(fog)
	_add(managers["Server"], CRATE, "Table", Vector2.ZERO)
	assert_true(await net.wait_until(func() -> bool: return _present(a, "World/Fog")), "A is admitted")
	net.start_clients()
	assert_true(await net.wait_connected())
	assert_true(await net.wait_until(func() -> bool: return _present(b, "World/Table")), "the late joiner's burst")
	await _frames(15)
	assert_false(b.has_node("World/Fog"), "the burst leaves out what the filter refuses")
	allowed.append(net.peer_id(b))
	assert_true(await net.wait_until(func() -> bool: return _present(b, "World/Fog")), "admitted later: it arrives")
	assert_true(_present(a, "World/Fog"))
