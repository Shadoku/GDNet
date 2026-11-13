extends TestCase
## WP16 (docs/PLAN_NETCODE.md 12.2, gate G9): several rooms per peer, level placement and auto-travel,
## under NetFixture. The fixtures are in tests/fixtures/rooms/: two chunks that name each other
## (`chunk_b` authored at x = 1000, the edge between them at x = 500, both with `auto_travel` and
## `bounds`), two interiors with a floor, a crate, a lamp and an `Entrance` marker, and a 3D vault.

const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const A := "res://tests/fixtures/rooms/chunk_a.tscn"
const B := "res://tests/fixtures/rooms/chunk_b.tscn"
const IN_A := "res://tests/fixtures/rooms/interior_a.tscn"
const IN_B := "res://tests/fixtures/rooms/interior_b.tscn"
const VAULT := "res://tests/fixtures/rooms/vault_3d.tscn"
const SPACING := "gdnet/levels/spacing"
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]
## The fixture player's speed (px/s) and the most it moves in one tick at 60 Hz.
const SPEED := 200.0

var net: NetFixture


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	ProjectSettings.set_setting(SPACING, null)
	if net != null:
		net.teardown()
		net = null


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 11
	return e


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## The server loads `level` and spawns a player for every player peer under its manager, from `at`
## downwards in steps of 40 px. Returns the server's players by peer id.
func _spawn_players(f: NetFixture, level: String, at: Vector2) -> Dictionary:
	var root: Node = f.server_session.load_level(level)
	var out := {}
	var y := 0.0
	for id in f.players():
		var p: Node2D = Net.spawn_for(id, PLAYER, root.get_node("NetManager"), "P%d" % id)
		p.position = at + Vector2(0.0, y)
		y += 40.0
		out[id] = p
	return out


func _player(branch: Node, id: int) -> Node2D:
	return branch.find_child("P%d" % id, true, false) as Node2D


func _branches(f: NetFixture) -> Array:
	return [f.server] + f.clients


## Every player is present in every branch (the replicas arrived).
func _all_present(f: NetFixture) -> bool:
	for branch: Node in _branches(f):
		for id in f.players():
			if _player(branch, id) == null:
				return false
	return true


## The level node a player hangs in (its manager's parent), "" when absent.
func _level_of(branch: Node, id: int) -> String:
	var p := _player(branch, id)
	return "" if p == null else String(p.get_parent().get_parent().name)


func _has_levels(s: NetSession, paths: Array) -> bool:
	var have := Array(s.levels())
	for p in paths:
		if not have.has(p):
			return false
	return true


func _sorted(a: PackedStringArray) -> Array:
	var out := Array(a)
	out.sort()
	return out


func test_a_neighbour_room_is_loaded_and_seen() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		_spawn_players(f, A, Vector2.ZERO)
		assert_true(await f.wait_until(func() -> bool: return _has_levels(f.session(ca), [A, B])), "the client loaded its chunk and the neighbour")
		assert_true(await f.wait_until(func() -> bool:
			return ca.has_node("chunk_b/NetManager/TreeB") and Net.net_id(ca.get_node("chunk_b/NetManager/TreeB")) != 0 \
				and Net.net_id(ca.get_node("chunk_a/NetManager/TreeA")) != 0), "it holds the entities of both")
		assert_has(f.server_session.levels(), B, "the server loaded the neighbour as well")
		assert_eq(Array(f.server_session.rooms(a)), [A, B], "its rooms: its player's first, then the neighbour")
		assert_eq(f.server_session.room_of(a), A)
		assert_true(await f.wait_until(func() -> bool: return f.session(ca).room_of(a) == A), "the client knows its own room")
		assert_eq(_sorted(f.session(ca).rooms(a)), [A, B])
		var st: Dictionary = f.session(ca).stats()
		assert_eq(st["room"], A, "stats: the client's room")
		assert_eq(_sorted(st["my_rooms"]), [A, B], "stats: all its rooms")
		assert_eq(st["levels"], 2)
		if f.host_mode == NetFixture.HostMode.HOST_PLAYER:
			assert_eq(Array(f.server_session.stats()["my_rooms"]), [A, B], "stats on the host: its own player's rooms")
		assert_eq((ca.get_node("chunk_b") as Node2D).position, Vector2(1000, 0), "a chunk keeps its authored position")
		assert_null(f.session(ca).level_origin(B), "and has no origin")
	)
	assert_true(ok, "fixtures connected")


func test_crossing_a_chunk_edge_frees_nothing_and_never_rolls_back() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		for c: Node in f.clients:
			f.set_emulation(c, _link(30.0))
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		_spawn_players(f, A, Vector2(440, -100))
		assert_true(await f.wait_until(func() -> bool: return _all_present(f)), "every player everywhere")
		await _frames(60)
		var ids := {}
		for branch: Node in _branches(f):
			for id in f.players():
				ids["%s/%d" % [branch.name, id]] = _player(branch, id).get_instance_id()
		var rolled := []
		for c: Node in f.clients:
			var own := _player(c, f.peer_id(c))
			own.get_node("NetEntity").rolled_back.connect(func(from_tick: int, to_tick: int) -> void: rolled.append([c.name, from_tick, to_tick]))
		var hops := []
		var own_a := _player(ca, a)
		f.session(ca).travelled.connect(func(p: Node, from: String, to: String) -> void:
			if p == own_a:
				hops.append([from, to]))
		var free_before: int = f.session(ca).stats()["scope_left"]
		var tracked: Array[Node2D] = [_player(ca, a), _player(f.server, a)]
		var last: Array[Vector2] = [tracked[0].global_position, tracked[1].global_position]
		var max_step := [0.0, 0.0]
		Input.action_press("right")
		for i in 50:
			await get_tree().physics_frame
			for k in 2:
				max_step[k] = maxf(max_step[k], tracked[k].global_position.distance_to(last[k]))
				last[k] = tracked[k].global_position
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool:
			for branch: Node in _branches(f):
				for id in f.players():
					if _level_of(branch, id) != "chunk_b":
						return false
			return true), "every player crossed into chunk B on every peer")
		for branch: Node in _branches(f):
			for id in f.players():
				assert_eq(_player(branch, id).get_instance_id(), ids["%s/%d" % [branch.name, id]], "%s's copy of %d was moved, not recreated" % [branch.name, id])
		assert_eq(rolled, [], "the move caused no rollback")
		assert_eq(f.session(ca).stats()["scope_left"], free_before, "nothing left A's scope")
		assert_eq(hops, [[A, B]], "A's client saw its player travel once")
		var tick_step := SPEED / Engine.physics_ticks_per_second
		assert_lt(max_step[0], tick_step * 2.0 + 0.5, "the owner's position is continuous")
		assert_lt(max_step[1], tick_step * 2.0 + 0.5, "the server's position is continuous")
		assert_gt(_player(f.server, a).global_position.x, 500.0)
		assert_eq(f.server_session.room_of(a), B)
		assert_eq(_sorted(f.server_session.rooms(a)), [A, B])
	)
	assert_true(ok, "fixtures connected")


func test_travel_between_two_rooms_a_peer_has_is_only_a_move() -> void:
	var ok := await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var cb: Node = f.clients[1]
		var a := f.peer_id(ca)
		for level in [A, B]:
			(f.server_session.load_level(level).get_node("NetManager") as NetManager).auto_travel = false
		var players := _spawn_players(f, A, Vector2(100, 0))
		assert_true(await f.wait_until(func() -> bool: return _all_present(f)), "every player everywhere")
		await _frames(30)
		var mine := _player(ca, a)
		var theirs := _player(cb, a)
		var at := mine.global_position
		var rolled := []
		mine.get_node("NetEntity").rolled_back.connect(func(from_tick: int, to_tick: int) -> void: rolled.append([from_tick, to_tick]))
		var hops := []
		f.session(cb).travelled.connect(func(p: Node, from: String, to: String) -> void:
			if p == theirs:
				hops.append([from, to]))
		f.server_session.travel(players[a], B)
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "chunk_b" and _level_of(cb, a) == "chunk_b"), "the move reached both clients")
		assert_eq(_player(ca, a), mine, "the owner's node was moved")
		assert_eq(_player(cb, a), theirs, "the other client's replica was moved, not freed")
		assert_eq(hops, [[A, B]], "and it saw the travel")
		await _frames(10)
		assert_eq(mine.global_position, at, "without a spawn point the player keeps its position")
		assert_eq((players[a] as Node2D).global_position, at)
		assert_eq(rolled, [])
		# Back to A while running, over a 30 ms link: still a move, continuous, no rollback.
		for c: Node in f.clients:
			f.set_emulation(c, _link(30.0))
		await _frames(30)
		Input.action_press("right")
		await _frames(10)
		f.server_session.travel(players[a], A)
		var last := mine.global_position
		var max_step := 0.0
		for i in 30:
			await get_tree().physics_frame
			max_step = maxf(max_step, mine.global_position.distance_to(last))
			last = mine.global_position
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "chunk_a" and _level_of(cb, a) == "chunk_a"), "the second move reached both clients")
		assert_eq(_player(ca, a), mine)
		assert_eq(_player(cb, a), theirs)
		assert_lt(max_step, SPEED / Engine.physics_ticks_per_second * 2.0 + 0.5, "the owner's position is continuous")
		assert_eq(rolled, [], "the move caused no rollback")
	)
	assert_true(ok, "fixtures connected")


func test_add_room_and_remove_room_load_and_free_the_level() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		_spawn_players(f, A, Vector2.ZERO)
		assert_true(await f.wait_until(func() -> bool: return _has_levels(f.session(ca), [A, B])))
		assert_false(Array(f.session(ca).levels()).has(IN_A))
		f.server_session.add_room(a, IN_A)
		assert_true(await f.wait_until(func() -> bool:
			return ca.has_node("interior_a/NetManager/Lamp") and Net.net_id(ca.get_node("interior_a/NetManager/Lamp")) != 0), "the added room's level and entities reached the client")
		assert_has(f.server_session.levels(), IN_A, "the server loaded it")
		assert_has(Array(f.server_session.rooms(a)), IN_A)
		f.server_session.remove_room(a, IN_A)
		assert_true(await f.wait_until(func() -> bool: return not ca.has_node("interior_a")), "the client freed the level")
		assert_false(Array(f.session(ca).levels()).has(IN_A))
		assert_true(_has_levels(f.session(ca), [A, B]), "and kept its chunks")
		assert_eq(_level_of(ca, a), "chunk_a")
	)
	assert_true(ok, "fixtures connected")


func test_spacing_places_levels_apart_the_same_on_every_peer() -> void:
	ProjectSettings.set_setting(SPACING, 4000.0)
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		_spawn_players(f, A, Vector2.ZERO)
		f.server_session.add_room(a, IN_A)
		f.server_session.add_room(a, IN_B)
		assert_true(await f.wait_until(func() -> bool: return _has_levels(f.session(ca), [A, B, IN_A, IN_B])))
		var oa: Variant = f.server_session.level_origin(IN_A)
		var ob: Variant = f.server_session.level_origin(IN_B)
		assert_eq(oa, Vector2(4000, 0), "the first placed level takes the first cell")
		assert_eq(ob, Vector2(8000, 0), "the second the next one")
		assert_null(f.server_session.level_origin(A), "a chunk naming neighbours keeps its authored position")
		for branch: Node in _branches(f):
			assert_eq((branch.get_node("interior_a") as Node2D).position, oa, "%s: interior A at its origin" % branch.name)
			assert_eq((branch.get_node("interior_b") as Node2D).position, ob, "%s: interior B at its origin" % branch.name)
			assert_eq((branch.get_node("chunk_b") as Node2D).position, Vector2(1000, 0))
		assert_eq(f.session(ca).level_origin(IN_A), oa)
		assert_eq(f.session(ca).level_origin(IN_B), ob)
		var lamp := ca.get_node("interior_b/NetManager/Lamp") as Node2D
		assert_eq(lamp.global_position, ob + Vector2(-100, -50), "the level's entities moved with it")
		# A freed cell is reused.
		f.server_session.remove_room(a, IN_A)
		assert_true(await f.wait_until(func() -> bool: return not ca.has_node("interior_a")))
		assert_true(f.server_session.unload_level(IN_A))
		f.server_session.empty_level_timeout = 1000.0
		var again: Node2D = f.server_session.load_level(IN_A)
		assert_eq(again.position, Vector2(4000, 0), "the freed cell is taken again")
	)
	assert_true(ok, "fixtures connected")


func test_bodies_of_two_placed_interiors_do_not_collide() -> void:
	ProjectSettings.set_setting(SPACING, 4000.0)
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	var la: Node2D = net.server_session.load_level(IN_A)
	var lb: Node2D = net.server_session.load_level(IN_B)
	assert_ne(la.position, lb.position)
	var ca := net.server.get_node("interior_a/NetManager/Crate") as RigidBody2D
	var cb := net.server.get_node("interior_b/NetManager/Crate") as RigidBody2D
	assert_eq(ca.global_position, la.position, "the crate moved with its level")
	await _frames(90)
	for pair: Array in [[ca, la], [cb, lb]]:
		var local: Vector2 = (pair[0] as Node2D).global_position - (pair[1] as Node2D).position
		assert_almost_eq(local.x, 0.0, 0.5, "the crate fell straight down: nothing pushed it sideways")
		assert_almost_eq(local.y, 80.0, 1.0, "it rests on its own level's floor")


func test_an_explicit_origin_and_spawn_markers() -> void:
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var origin := Vector2(-3000, 800)
		var level: Node2D = f.server_session.load_level(IN_A, origin)
		assert_eq(level.position, origin)
		assert_eq(f.server_session.level_origin(IN_A), origin)
		assert_eq((level.get_node("NetManager/Lamp") as Node2D).global_position, origin + Vector2(-100, -50))
		var players := _spawn_players(f, A, Vector2.ZERO)
		f.server_session.travel(players[a], IN_A, "Entrance")
		assert_true(await f.wait_until(func() -> bool: return _level_of(ca, a) == "interior_a"), "the player travelled")
		var spot := origin + Vector2(50, 20)
		assert_eq((players[a] as Node2D).global_position, spot, "it stands at the marker, origin applied")
		assert_eq((ca.get_node("interior_a") as Node2D).position, origin, "the client placed the level at the same origin")
		assert_eq(f.session(ca).level_origin(IN_A), origin)
		assert_true(await f.wait_until(func() -> bool: return _player(ca, a).global_position.distance_to(spot) < 0.2), "the client's player is there too")
		assert_eq((ca.get_node("interior_a/NetManager/Lamp") as Node2D).global_position, origin + Vector2(-100, -50))
	)
	assert_true(ok, "fixtures connected")


func test_a_3d_level_takes_a_vector3_origin_and_a_cell() -> void:
	ProjectSettings.set_setting(SPACING, 50.0)
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	var v: Node3D = net.server_session.load_level(VAULT, Vector3(100, 0, 20))
	assert_eq(v.position, Vector3(100, 0, 20))
	assert_eq((v.get_node("NetManager/Box") as Node3D).global_position, Vector3(101, 2, 23))
	assert_eq(net.server_session.level_origin(VAULT), Vector3(100, 0, 20))
	assert_true(net.server_session.unload_level(VAULT))
	var w: Node3D = net.server_session.load_level(VAULT)
	assert_eq(w.position, Vector3(50, 0, 0), "without an origin it takes the first cell")
