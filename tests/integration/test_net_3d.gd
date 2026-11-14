extends TestCase
## WP18 3D parity (docs/PLAN_NETCODE.md 12.5, gate G10): the 3D twins of the 2D scenario families of
## state (WP3) and prediction (WP4), in both host modes (G2). Interpolation is in test_net_3d_interp.gd;
## calls, authority, interest, rooms, seats and the overlay in test_net_3d_world.gd; bodies and
## interaction scoring in test_net_3d_physics.gd. Fixtures: tests/fixtures/net3d/. 3D lengths are
## metres (decision D17): `mm` is a 1 mm step, so a replica holds a position to half of it.

const CRATE := preload("res://tests/fixtures/net3d/crate_3d.tscn")
const PLAYER := preload("res://tests/fixtures/net3d/player_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]

## Half a `mm` step (0.5 mm) on each of three axes, as a distance: the most a replica's quantised copy
## of a `Vector3` differs from the server's value (0.0005 x sqrt(3)).
const HALF_STEP := 0.00087
## The most a `smallest3` quaternion's round trip turns a rotation, in radians (0.0086 degrees): the
## three sent components are 15-bit steps over [-1/sqrt(2), 1/sqrt(2)], so each is off by at most half a
## step, 2.2e-5; the derived largest one (at least 0.5) by at most 6.5e-5; the rotation between the two
## quaternions is at most twice the length of that error, 2 x 7.5e-5. Measured: under 7e-5 (guide 03).
const ANGLE_BOUND := 1.5e-4
## One `mm` step: the default prediction tolerance of a `Vector3` (0.001 m). The owner resumes from the
## server's quantised state, so after a correction it sits within half a step on each axis.
const MM := 0.001

var net: NetFixture


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 7
	return e


## The rotation between two quaternions in radians, double cover included (q and -q are one rotation),
## computed so that angles of 1e-6 resolve (Quaternion.angle_to's acos of a 32-bit dot does not).
func _angle(a: Quaternion, b: Quaternion) -> float:
	var d := a.inverse() * b
	return 2.0 * asin(minf(1.0, Vector3(d.x, d.y, d.z).length()))


## A rotation whose largest component is negative (Godot gives it with w > 0, the quantiser sends -q).
func _turned() -> Quaternion:
	return Quaternion(Vector3(1.0, 2.0, -3.0).normalized(), 2.5)


# --- state (WP3) ----------------------------------------------------------------------------------


func test_state_a_3d_entity_reaches_the_client_within_the_quantiser_bounds() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var c: Node3D = CRATE.instantiate()
		c.name = "Crate"
		c.position = Vector3(1.2345, -2.5, 3.7501)
		c.quaternion = _turned()
		c.scale = Vector3(1.5, 2.0, 0.75)
		c.hp = 42
		(c.get_node("Arm") as Node3D).position = Vector3(0.25, 1.5, -0.75)
		managers["Server"].add_child(c)
		var seen := {}
		f.session(client).entity_spawned.connect(func(n: Node) -> void: seen.merge(n.at_spawn))
		assert_true(await f.wait_until(func() -> bool: return seen.has("position")), "spawned on the client")
		# The initial state, cold and hot, nested part included, was in place before _net_spawned.
		assert_lt((seen["position"] as Vector3).distance_to(c.position), HALF_STEP, "position at spawn: %s" % seen["position"])
		assert_lt(_angle(seen["quaternion"], c.quaternion), ANGLE_BOUND, "rotation at spawn")
		assert_true((seen["scale"] as Vector3).is_equal_approx(c.scale), "scale at spawn: %s" % seen["scale"])
		assert_eq(seen["hp"], 42, "the @export cold property at spawn")
		assert_lt((seen["arm"] as Vector3).distance_to(Vector3(0.25, 1.5, -0.75)), HALF_STEP, "the nested part at spawn")
		# Changes, every family of value.
		var r: Node3D = client.get_node("World/Crate")
		for i in 40:
			c.position += Vector3(0.0123, -0.0071, 0.031)
			c.quaternion = (Quaternion(Vector3(0.3, 1.0, 0.2).normalized(), 0.21) * c.quaternion).normalized()
			(c.get_node("Arm") as Node3D).position.x += 0.01
			await get_tree().physics_frame
		c.scale = Vector3(0.5, 0.5, 3.0)
		c.hp = 7
		var converged := func() -> bool:
			return r.position.distance_to(c.position) < HALF_STEP and _angle(r.quaternion, c.quaternion) < ANGLE_BOUND \
				and r.scale.is_equal_approx(c.scale) and r.hp == 7 \
				and (r.get_node("Arm") as Node3D).position.distance_to((c.get_node("Arm") as Node3D).position) < HALF_STEP
		assert_true(await f.wait_until(converged, 3.0), "the replica converged: %s vs %s, %s vs %s, %s, hp %d" % [r.position, c.position, r.quaternion, c.quaternion, r.scale, r.hp])
		# Hash telemetry compares the quantised hot fields: a healthy 3D entity never diverges.
		var cs: Dictionary = f.session(client).stats()
		assert_gt(cs["hashes_compared"], 0, "hashes were compared")
		assert_eq(cs["divergences"], 0, "and none diverged")
		print("3D state (mode %d): position error %.6f m, rotation error %.2f microrad" % [f.host_mode, r.position.distance_to(c.position), _angle(r.quaternion, c.quaternion) * 1e6]))
	assert_true(ok)


## The quantiser's angular error over many random rotations: the bound the other tests assert.
func test_state_smallest3_turns_a_rotation_by_at_most_the_bound() -> void:
	var net_a := NetFixture.new(self, 1)
	net = net_a
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var crates: Array[Node3D] = []
	var rng := RandomNumberGenerator.new()
	rng.seed = 18
	for i in 200:
		var c: Node3D = CRATE.instantiate()
		c.name = "Q%d" % i
		var axis := Vector3(rng.randf_range(-1, 1), rng.randf_range(-1, 1), rng.randf_range(-1, 1)).normalized()
		c.quaternion = Quaternion(axis, rng.randf_range(-PI, PI))
		managers["Server"].add_child(c)
		crates.append(c)
	var client: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool: return net.session(client).entity_count() == 200))
	await _frames(5)
	var worst := 0.0
	for c in crates:
		worst = maxf(worst, _angle((client.get_node("World/" + String(c.name)) as Node3D).quaternion, c.quaternion))
	print("smallest3: the largest rotation error of 200 random rotations %.2f microrad (%.5f degrees)" % [worst * 1e6, rad_to_deg(worst)])
	assert_lt(worst, ANGLE_BOUND)


func test_state_a_late_joiner_receives_the_current_3d_state() -> void:
	net = NetFixture.new(self, 1)
	net.start_server()
	var managers := net.add_managers()
	var crates: Array[Node3D] = []
	for i in 5:
		var c: Node3D = CRATE.instantiate()
		c.name = "Mover%d" % i
		managers["Server"].add_child(c)
		crates.append(c)
	var mover := func(_t: int) -> void:
		for i in crates.size():
			crates[i].position += Vector3(0.01 * (i + 1), 0.005, -0.02)
			crates[i].quaternion = (Quaternion(Vector3.UP, 0.05) * crates[i].quaternion).normalized()
	net.server_session.ticked.connect(mover)
	await _frames(30)
	net.server_session.ticked.disconnect(mover)
	net.start_clients()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool:
		for c in crates:
			var r := client.get_node_or_null("World/" + String(c.name)) as Node3D
			if r == null or r.position.distance_to(c.position) > HALF_STEP or _angle(r.quaternion, c.quaternion) > ANGLE_BOUND:
				return false
		return true), "every entity's position and rotation match after the burst")
	assert_gt(crates[0].position.length(), 0.5, "they had moved before the client joined")


# --- prediction (WP4) -----------------------------------------------------------------------------


## Starts a fixture over a 50 ms link with a floor under everything; returns the managers.
func _start_on_floor(clients: int, mode: NetFixture.HostMode) -> Dictionary:
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers()


## Spawns the 3D player for `peer`, waits for every copy and a second of pings, and returns the copies
## by branch name, with each copy's rollbacks collected in its `rolled` meta.
func _spawn_player(managers: Dictionary, peer: int, node_name: String = "P") -> Dictionary:
	var sp: Node = Net.spawn_for(peer, PLAYER, managers["Server"], node_name)
	var out := {"Server": sp}
	for c in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path)), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	await _frames(Engine.physics_ticks_per_second)
	for k: String in out:
		var list: Array = []
		out[k].set_meta("rolled", list)
		(out[k].get_node("NetEntity") as NetEntity).rolled_back.connect(func(a: int, b: int) -> void: list.append([a, b]))
	return out


func _rolled(n: Node) -> Array:
	return n.get_meta("rolled")


## Ticks until the two are within one mm step on each axis (-1: never within `limit`).
func _ticks_to_converge(a: Node3D, b: Node3D, limit: int) -> int:
	for t in limit:
		if _within(a.position, b.position, MM):
			return t
		await get_tree().physics_frame
	return -1


func _within(a: Vector3, b: Vector3, tol: float) -> bool:
	var d := (a - b).abs()
	return d.x <= tol and d.y <= tol and d.z <= tol


func test_prediction_a_3d_player_moves_at_once_converges_and_rolls_back_once_per_impulse() -> void:
	timeout_sec = 30.0
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var managers := await _start_on_floor(1, mode)
		var cid := net.peer_id(net.clients[0])
		var copies := await _spawn_player(managers, cid)
		var sp: CharacterBody3D = copies["Server"]
		var cp: CharacterBody3D = copies["ClientA"]
		assert_true(sp.is_on_floor() and cp.is_on_floor(), "both stand on the floor")
		var start := cp.position
		assert_true(_within(sp.position, start, MM), "both copies start in the same place")
		# Walk: the owner moves on the tick after the press, the server follows.
		await get_tree().physics_frame
		Input.action_press("right")
		await get_tree().physics_frame
		assert_gt(cp.position.x, start.x, "the owner moved on the very tick after the press")
		assert_almost_eq(sp.position.x, start.x, MM, "the server has not received the input yet")
		await _frames(30)
		assert_gt(sp.position.x, start.x, "the server's copy follows")
		Input.action_release("right")
		var n := await _ticks_to_converge(sp, cp, 120)
		assert_ne(n, -1, "the copies converged (server %s, owner %s)" % [sp.position, cp.position])
		assert_gt(cp.position.x, start.x + 1.5, "the owner walked about two metres")
		# Jump: up on the next tick, back on the floor, the server matches.
		Input.action_press("jump")
		await get_tree().physics_frame
		await get_tree().physics_frame
		assert_gt(cp.position.y, start.y + 0.01, "the owner jumped at once")
		Input.action_release("jump")
		assert_true(await net.wait_until(func() -> bool: return sp.jumps == 1 and sp.is_on_floor() and cp.is_on_floor() and _within(sp.position, cp.position, MM), 3.0), "both landed in the same place")
		assert_eq(cp.jumps, 1, "one jump on the owner")
		assert_eq(_rolled(cp).size(), 0, "no rollback while the prediction is right: %s" % [_rolled(cp)])
		# A server impulse: exactly one rollback, then converged.
		sp.position.x += 1.0
		var m := await _ticks_to_converge(sp, cp, 120)
		assert_ne(m, -1, "the owner took the server's correction")
		await _frames(30)
		assert_eq(_rolled(cp).size(), 1, "exactly one rollback: %s" % [_rolled(cp)])
		assert_eq(_rolled(sp).size(), 0, "the server never rolls back")
		var stats: Dictionary = net.client_sessions[0].stats()
		print("3D prediction (mode %d): converged %d ticks after the release, %d after a 1 m impulse; lead %d ticks; rollbacks %d; replayed %d" % [mode, n, m, stats["prediction_lead"], stats["rollbacks"], stats["replayed_ticks"]])
		net.teardown()
		net = null
