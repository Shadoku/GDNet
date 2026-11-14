extends TestCase
## WP18 3D parity (docs/PLAN_NETCODE.md 12.5, gate G10): the 3D twins of calls (WP6), authority (WP7),
## interest (WP16), rooms and level state (WP13, WP16) and seats (WP17), plus the overlay with 3D
## entities, in both host modes (G2). Fixtures: tests/fixtures/net3d/. Lengths are metres (D17).

const CRATE := preload("res://tests/fixtures/net3d/crate_3d.tscn")
const PLAYER := preload("res://tests/fixtures/net3d/player_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const PROBE := preload("res://tests/fixtures/net3d/call_probe_3d.tscn")
const BOX := preload("res://tests/fixtures/net3d/owned_box_3d.tscn")
const MARKER := preload("res://tests/fixtures/net3d/radius_marker_3d.tscn")
const COIN := preload("res://tests/fixtures/net3d/coin_3d.tscn")
const LOBBY := "res://tests/fixtures/rooms/vault_3d.tscn"
const HALL := "res://tests/fixtures/net3d/hall_3d.tscn"
const SPACING := "gdnet/levels/spacing"
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]
## Half a `mm` step on three axes (0.0005 x sqrt(3)): the most a replica's position differs.
const HALF_STEP := 0.00087
## One `mm` step, the prediction tolerance of a `Vector3`.
const MM := 0.001
## The most a `smallest3` round trip turns a rotation (radians; derived in test_net_3d.gd).
const ANGLE_BOUND := 1.5e-4

var net: NetFixture
var _project_spacing: Variant = null


func before_each() -> void:
	_project_spacing = ProjectSettings.get_setting(SPACING, null)
	ProjectSettings.set_setting(SPACING, null)


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	ProjectSettings.set_setting(SPACING, _project_spacing)
	Net.show_overlay(false)
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## The rotation between two quaternions in radians, q and -q being one rotation.
func _angle(a: Quaternion, b: Quaternion) -> float:
	var d := a.inverse() * b
	return 2.0 * asin(minf(1.0, Vector3(d.x, d.y, d.z).length()))


func _present(branch: Node, path: String) -> bool:
	return branch != null and branch.has_node(path) and Net.net_id(branch.get_node(path)) > 0


## Adds `scene` named `node_name` owned by `owner` under the server's manager; returns its copies by
## branch name once every client has it.
func _spawn(f: NetFixture, managers: Dictionary, scene: PackedScene, node_name: String, owner: int) -> Dictionary:
	var node: Node = scene.instantiate()
	node.name = node_name
	Net.set_owner(node, owner)
	managers["Server"].add_child(node)
	var out := {"Server": node}
	for c in f.clients:
		var path := "World/" + node_name
		assert_true(await f.wait_until(func() -> bool: return _present(c, path)), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	return out


# --- calls and authority (WP6, WP7) ---------------------------------------------------------------


func test_calls_a_3d_entity_takes_vector3_quaternion_basis_and_transform3d_arguments() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var a: Node = f.clients[0]
		var probes := await _spawn(f, f.add_managers(), PROBE, "Probe", f.peer_id(a))
		var v := Vector3(1.25, -2.5, 3.125)
		var q := Quaternion(Vector3(1, 2, -3).normalized(), 2.5)
		var b := Basis(q).scaled(Vector3(1, 2, 0.5))
		var t := Transform3D(Basis(Vector3(0, 1, 0), 0.75), Vector3(4, 5, -6))
		var call: NetCall = Net.sync(probes["ClientA"].place, v, q, b, t)
		assert_true(await f.wait_until(func() -> bool: return probes["ClientA"].calls.size() == 1), "it came back to the caller")
		for k: String in probes:
			assert_eq(probes[k].calls, [["place", v, q, b, t]], "the exact values on " + k)
		assert_true(call.ok, "the call resolved: %s" % call.error)
		var back: Transform3D = await Net.server(probes["ClientA"].mirrored, t)
		assert_eq(back, t.rotated(Vector3.UP, PI), "a Transform3D comes back as the result")
		assert_true(await f.wait_until(func() -> bool: return (probes["ClientA"] as Node3D).position.distance_to(Vector3(4, 5, -6)) < HALF_STEP), "the state the call set replicates"))
	assert_true(ok)


func test_authority_set_owner_moves_prediction_of_a_3d_player() -> void:
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var a := f.peer_id(f.clients[0])
		var b := f.peer_id(f.clients[1])
		var copies := await _spawn(f, f.add_managers(), PLAYER, "P", a)
		await _frames(10)
		assert_true(copies["ClientA"].is_physics_processing(), "A predicts its player")
		assert_false(copies["ClientB"].is_physics_processing(), "B shows it")
		assert_true(Net.set_owner(copies["Server"], b))
		assert_true(await f.wait_until(func() -> bool:
			return copies["ClientB"].is_physics_processing() and not copies["ClientA"].is_physics_processing()), "B predicts now, A shows it")
		await _frames(20)
		var start: Vector3 = copies["ClientB"].position
		Input.action_press("right")
		await _frames(2)
		assert_gt(copies["ClientB"].position.x, start.x, "B's press moves it at once")
		await _frames(20)
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool:
			var s: Vector3 = copies["Server"].position
			return s.x > start.x + 1.0 and s.distance_to(copies["ClientB"].position) < MM and s.distance_to(copies["ClientA"].position) < HALF_STEP), "the server and A follow B"))
	assert_true(ok)


func test_authority_a_client_owned_3d_box_moved_by_its_owner_reaches_everyone() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var copies := await _spawn(f, f.add_managers(), BOX, "Box", f.peer_id(f.clients[0]))
		var mine: Node3D = copies["ClientA"]
		await _frames(5)
		assert_true(mine.is_physics_processing(), "the owner simulates it")
		assert_false(copies["Server"].is_physics_processing(), "the server does not")
		mine.seen.clear()
		mine.speed = Vector3(1.2, 0.3, -0.6)
		mine.spin = 2.0
		await _frames(40)
		for i in range(1, mine.seen.size()):
			assert_lt((mine.seen[i] - mine.seen[i - 1] - mine.speed / 60.0).length(), 0.00001, "moved by its own script only, frame %d" % i)
		mine.speed = Vector3.ZERO
		mine.spin = 0.0
		assert_true(await f.wait_until(func() -> bool:
			for k: String in ["Server", "ClientB"]:
				var n: Node3D = copies[k]
				if n.position.distance_to(mine.position) > HALF_STEP or _angle(n.quaternion, mine.quaternion) > ANGLE_BOUND:
					return false
			return true), "the server and the other client settle on the owner's transform")
		assert_eq(f.client_sessions[0].stats()["writes_rejected"], 0))
	assert_true(ok)


# --- interest (WP16) ------------------------------------------------------------------------------


func test_interest_a_radius_in_metres_with_hysteresis_and_a_3d_focus() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var world: Node = managers["Server"]
		var client: Node = f.clients[0]
		var cid := f.peer_id(client)
		Net.spawn_for(cid, CRATE, world, "Player")
		var far: Node3D = MARKER.instantiate()
		far.name = "Far"
		far.position = Vector3(0, 0, 3)
		world.add_child(far)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Player")))
		await _frames(10)
		assert_false(client.has_node("World/Far"), "3 m away with a 2 m radius: absent")
		far.position = Vector3(1.2, 1.0, 1.2)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Far")), "1.9 m away: present")
		var copy: Node = client.get_node("World/Far")
		# Between r and 1.2 r (2.0 .. 2.4 m) it stays.
		for p: Vector3 in [Vector3(0, 0, 2.1), Vector3(0, 2.3, 0), Vector3(1.6, 0, 1.6), Vector3(0, 0, -2.35)]:
			far.position = p
			await _frames(3)
		assert_true(is_instance_valid(copy) and client.has_node("World/Far"), "still there at 2.35 m")
		far.position = Vector3(0, 0, 2.5)
		assert_true(await f.wait_until(func() -> bool: return not client.has_node("World/Far")), "beyond 1.2 r it left")
		# A Node3D far away is the focus now.
		far.position = Vector3(50, 0, 0)
		var cam := Node3D.new()
		cam.position = Vector3(50, 1, 1)
		f.server.add_child(cam)
		f.server_session.set_focus(cid, cam)
		assert_true(await f.wait_until(func() -> bool: return _present(client, "World/Far")), "near the 3D focus: present")
		f.server_session.set_focus(cid, null)
		assert_true(await f.wait_until(func() -> bool: return not client.has_node("World/Far")), "back to the player: absent")
		cam.free())
	assert_true(ok)


# --- rooms and level state (WP13, WP16) -----------------------------------------------------------


## Where the test drops a coin in the hall, in the hall's own coordinates.
const DROP := Vector3(0.5, 0.125, 2.25)


## "" when the hall shows the play of the test (chest open, coin taken, a coin dropped at `DROP` in the
## level's own coordinates, wherever the level was placed), else what differs.
func _kept(level: Node) -> String:
	if level == null:
		return "no level"
	var m := level.get_node("NetManager")
	if not m.has_node("Chest") or not m.get_node("Chest").open:
		return "the chest is not open"
	if m.has_node("Coin"):
		return "the coin is back"
	var dropped := m.get_node_or_null("Dropped") as Node3D
	if dropped == null or dropped.value != 7:
		return "the dropped coin is missing"
	if dropped.global_position.distance_to((level as Node3D).position + DROP) > HALF_STEP:
		return "the dropped coin is at %s in a level at %s" % [dropped.global_position, (level as Node3D).position]
	return ""


func test_rooms_travel_to_a_marker3d_and_a_3d_level_keeps_its_state() -> void:
	timeout_sec = 20.0
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var a := f.peer_id(ca)
		var s := f.server_session
		s.empty_level_timeout = 1000.0
		var lobby: Node = s.load_level(LOBBY)
		var origin := Vector3(40, 0, 0)
		var hall: Node3D = s.load_level(HALL, origin)
		var players := {}
		for id in f.players():
			players[id] = Net.spawn_for(id, CRATE, lobby.get_node("NetManager"), "P%d" % id)
		# A top_level node keeps its authored global place, moved with the level's origin.
		assert_eq((hall.get_node("NetManager/Chest/Beacon") as Node3D).global_position, origin + Vector3(0, 3, 0))
		# The hall's Entrance is turned a quarter about Y (2026-11-14: travel takes the marker's rotation);
		# the crate keeps its own scale.
		(players[a] as Node3D).scale = Vector3(1.5, 1.5, 1.5)
		s.travel(players[a], HALL, "Entrance")
		var spot := origin + Vector3(1.5, 0.25, -3)
		var facing := (hall.get_node("Entrance") as Node3D).global_basis.get_rotation_quaternion()
		assert_true(await f.wait_until(func() -> bool: return ca.has_node("hall_3d/NetManager/P%d" % a)), "the player travelled")
		assert_eq((players[a] as Node3D).global_position, spot, "it stands at the Marker3D, origin applied")
		assert_lt(_angle((players[a] as Node3D).quaternion, facing), 1e-5, "turned as the marker is")
		assert_lt((players[a] as Node3D).scale.distance_to(Vector3(1.5, 1.5, 1.5)), 1e-5, "with its own scale")
		var mine: Node3D = ca.get_node("hall_3d/NetManager/P%d" % a)
		assert_true(await f.wait_until(func() -> bool: return mine.global_position.distance_to(spot) < HALF_STEP), "the client's copy is there too")
		assert_true(await f.wait_until(func() -> bool: return _angle(mine.quaternion, facing) < ANGLE_BOUND and mine.scale.distance_to(Vector3(1.5, 1.5, 1.5)) < 0.01), "turned and scaled like the server's")
		assert_eq((ca.get_node("hall_3d") as Node3D).position, origin, "the client placed the hall at the same origin")
		assert_eq((ca.get_node("hall_3d/NetManager/Chest/Beacon") as Node3D).global_position, origin + Vector3(0, 3, 0), "and its top_level node")
		# Play the hall, leave it, let it unload, come back.
		var m := hall.get_node("NetManager")
		m.get_node("Chest").open = true
		m.get_node("Coin").queue_free()
		var dropped: Node3D = COIN.instantiate()
		dropped.name = "Dropped"
		dropped.value = 7
		# A manager is a plain Node: its children's positions are global, so the hall's origin is added.
		dropped.position = origin + DROP
		m.add_child(dropped)
		assert_true(await f.wait_until(func() -> bool: return Net.net_id(dropped) > 0 and _kept(ca.get_node("hall_3d")) == ""), "the client sees the play: %s" % _kept(ca.get_node("hall_3d")))
		s.travel(players[a], LOBBY, "Entrance")
		assert_true(await f.wait_until(func() -> bool: return lobby.has_node("NetManager/P%d" % a)), "back in the lobby")
		s.empty_level_timeout = 0.2
		assert_true(await f.wait_until(func() -> bool: return not Array(s.levels()).has(HALL)), "the empty hall unloads")
		s.empty_level_timeout = 1000.0
		assert_has(Array(s.kept_levels()), HALL, "its state is kept")
		# Loaded again at another origin (the authored place is inside the lobby's bounds_3d), with the
		# kept state.
		var origin2 := Vector3(-30, 2, 10)
		s.load_level(HALL, origin2)
		s.travel(players[a], HALL, "Entrance")
		assert_true(await f.wait_until(func() -> bool: return _present(ca, "hall_3d/NetManager/Chest")), "the client entered the hall again")
		assert_eq((players[a] as Node3D).global_position, origin2 + Vector3(1.5, 0.25, -3), "at the marker of the hall's new place")
		var again: Node = f.server.get_node("hall_3d")
		assert_eq(_kept(again), "", "the server's reloaded hall")
		assert_eq(again.get_node("NetManager/Chest").at_spawn, {"open": true, "value": 1}, "restored before _net_spawned")
		assert_true(await f.wait_until(func() -> bool: return _kept(ca.get_node("hall_3d")) == ""), "the client sees the kept state: %s" % _kept(ca.get_node("hall_3d"))))
	assert_true(ok)


## The owner's decision (2026-11-14): travel to a Marker3D gives the player the marker's global
## rotation (the basis without the marker's scale) and keeps the player's own scale; travel without a
## spawn point keeps the rotation. `quaternion` is a hot property of every Node3D, so the owner (rolled
## back to the server's pose) and another client in the room see it.
func test_rooms_travel_to_a_turned_marker3d_takes_its_rotation() -> void:
	timeout_sec = 20.0
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var ca: Node = f.clients[0]
		var cb: Node = f.clients[1]
		var a := f.peer_id(ca)
		var b := f.peer_id(cb)
		var s := f.server_session
		s.empty_level_timeout = 1000.0
		var lobby: Node = s.load_level(LOBBY)
		var hall: Node3D = s.load_level(HALL, Vector3(40, 0, 0))
		# Turned about all three axes and scaled: only its rotation is taken.
		var turned := Marker3D.new()
		turned.name = "Turned"
		turned.position = Vector3(-1, 0.5, 2)
		turned.basis = Basis.from_euler(Vector3(0.3, -2.0, 0.2)).scaled(Vector3(2, 2, 2))
		hall.add_child(turned)
		var want := turned.global_basis.get_rotation_quaternion()
		var players := {}
		for id in f.players():
			players[id] = Net.spawn_for(id, PLAYER, lobby.get_node("NetManager"), "P%d" % id)
		s.travel(players[b], HALL, "Entrance")
		assert_true(await f.wait_until(func() -> bool: return cb.has_node("hall_3d/NetManager/P%d" % b)), "the other player went in")
		var entrance := (hall.get_node("Entrance") as Node3D).global_basis.get_rotation_quaternion()
		assert_lt(_angle((players[b] as Node3D).quaternion, entrance), 1e-5, "it faces the way of the hall's Entrance")
		assert_true(await f.wait_until(func() -> bool: return ca.has_node("vault_3d/NetManager/P%d" % a)), "A has its player")
		var p: Node3D = players[a]
		s.travel(p, HALL, "Turned")
		var path := "hall_3d/NetManager/P%d" % a
		assert_true(await f.wait_until(func() -> bool: return ca.has_node(path) and cb.has_node(path)), "the player travelled")
		assert_lt(_angle(p.quaternion, want), 1e-5, "on the server it has the marker's rotation")
		assert_lt(p.scale.distance_to(Vector3.ONE), 1e-5, "and keeps its own scale (%s)" % p.scale)
		for c: Node in [ca, cb]:
			var copy: Node3D = c.get_node(path)
			assert_true(await f.wait_until(func() -> bool: return _angle(copy.quaternion, want) < ANGLE_BOUND), "%s: its copy is turned too" % c.name)
			assert_lt(copy.scale.distance_to(Vector3.ONE), 0.01, "%s: unscaled" % c.name)
		var mine: Node3D = ca.get_node(path)
		await _frames(10)
		assert_lt(_angle(mine.quaternion, want), ANGLE_BOUND, "the owner keeps the rotation")
		# Back to the lobby without a spawn point (once it stands on the floor): the rotation is kept.
		assert_true(await f.wait_until(func() -> bool: return (p as CharacterBody3D).is_on_floor()), "it landed")
		var at := p.global_position
		s.travel(p, LOBBY)
		assert_true(await f.wait_until(func() -> bool: return ca.has_node("vault_3d/NetManager/P%d" % a)), "back in the lobby")
		assert_lt(p.global_position.distance_to(at), 0.05, "without a spawn point it keeps its position")
		assert_lt(_angle(p.quaternion, want), 1e-5, "and its rotation")
		await _frames(10)
		assert_lt(_angle((ca.get_node("vault_3d/NetManager/P%d" % a) as Node3D).quaternion, want), ANGLE_BOUND, "on its owner too"))
	assert_true(ok)


# --- seats (WP17) ---------------------------------------------------------------------------------


func test_seats_the_owner_of_a_3d_predicted_body_drops_and_rejoins() -> void:
	timeout_sec = 20.0
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		f.server_session.rejoin_timeout_s = 30.0
		var managers := f.add_managers()
		var aid := f.peer_id(f.clients[0])
		var seat := f.server_session.seat(aid)
		var p: CharacterBody3D = Net.spawn_for(aid, PLAYER, managers["Server"], "P")
		assert_true(await f.wait_until(func() -> bool: return _present(f.clients[0], "World/P")))
		await _frames(20)
		Input.action_press("right")
		var x0 := p.position.x
		assert_true(await f.wait_until(func() -> bool: return p.position.x > x0 + 0.5), "the server's player walks")
		f.drop_client(0)
		assert_true(await f.wait_until(func() -> bool: return f.server_session.peer_of(seat) == 0), "the seat is away")
		await _frames(5)
		# Away: neutral input stops it, gravity still pulls it down.
		var stopped := p.position
		p.position.y += 1.0
		await _frames(40)
		assert_almost_eq(p.position.x, stopped.x, 0.001, "the dropped player stopped walking")
		assert_almost_eq(p.position.y, stopped.y, 0.01, "and fell back to the floor")
		assert_true(p.is_on_floor())
		Input.action_release("right")
		var a2 := f.rejoin_client(0)
		f.add_manager_to(a2)
		assert_true(await f.wait_until(func() -> bool: return f.session(a2).my_seat() == seat and _present(a2, "World/P")), "the same seat, the player again")
		var a2id := f.peer_id(a2)
		assert_eq(Net.get_owner_of(p), a2id, "the player belongs to the new id")
		var cp: CharacterBody3D = a2.get_node("World/P")
		assert_true(await f.wait_until(func() -> bool: return cp.is_physics_processing() and cp.position.distance_to(p.position) < MM), "the rejoined client predicts it from where it is")
		await _frames(30)
		var c0 := cp.position.x
		var s0 := p.position.x
		Input.action_press("right")
		await _frames(2)
		assert_gt(cp.position.x, c0, "the owner moves at once (prediction)")
		assert_true(await f.wait_until(func() -> bool: return p.position.x > s0 + 0.5), "the server follows")
		Input.action_release("right")
		assert_true(await f.wait_until(func() -> bool: return cp.position.distance_to(p.position) < MM), "they converge"))
	assert_true(ok)


# --- diagnostics (WP8) ----------------------------------------------------------------------------


func test_the_overlay_places_3d_entities_through_the_camera() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var cam := Camera3D.new()
	cam.position = Vector3(0, 2, 10)
	add_child(cam)
	cam.current = true
	await _spawn(net, managers, CRATE, "Seen", 1)
	var behind: Dictionary = await _spawn(net, managers, CRATE, "Behind", 1)
	(behind["Server"] as Node3D).position = Vector3(0, 0, 20)
	var client: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool: return (client.get_node("World/Behind") as Node3D).position.z > 19.0))
	assert_eq(net.client_sessions[0].overlay_rows().size(), 2, "both 3D entities have rows")
	Net.show_overlay(true)
	await _frames(2)
	var layer: Node = get_tree().root.get_node("__NetOverlay")
	var shown: Array[Label] = []
	for l: Label in layer.find_children("*", "Label", true, false):
		if l.visible:
			shown.append(l)
	assert_eq(shown.size(), 2, "the entity in front of the camera on both peers; the one behind it is not placed")
	var rect := get_viewport().get_visible_rect()
	var expected := cam.unproject_position(Vector3.ZERO) + Vector2(6, -18)
	for l in shown:
		assert_true(l.position.distance_to(expected) < 0.5, "placed where the camera sees the entity: %s vs %s" % [l.position, expected])
		assert_true(rect.has_point(l.position), "on screen")
