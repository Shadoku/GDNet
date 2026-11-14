extends TestCase
## WP19 slice A in 3D (docs/PLAN_NETCODE.md 12.4, gate G10 for the new modes): the 3D twins of the
## `owner` and `simulated` scenarios of `test_net_physics_modes.gd` and `test_net_physics_simulated.gd`:
## an owner's vehicle moves on the tick of its input and converges elsewhere, a push only the server's
## box got is corrected without a jump and a teleport snaps, a resting pile of boxes stays asleep and
## converges, and a predicted player on a swinging `simulated` platform does not roll back every tick.
## Lengths are metres. Every body entering a branch's manager goes to that branch's collision layer.

const BOX := preload("res://tests/fixtures/net3d/box_body_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const VEHICLE := preload("res://tests/fixtures/net3d/vehicle_3d.tscn")
const PLATFORM := preload("res://tests/fixtures/net3d/platform_3d.tscn")
const PLAYER := preload("res://tests/fixtures/net3d/player_3d.tscn")
const MODES := preload("res://tests/integration/test_net_physics_modes.gd")
## The default position threshold (0.02 m): below it a resting body is not corrected.
const THRESHOLD := 0.02
## A replica within half a `mm` step on each axis of the value sent.
const SETTLED := 0.001
const PILE := 32

var net: NetFixture


func after_each() -> void:
	Input.action_release("right")
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
	e.seed = 19
	return e


func _start(clients: int, mode: NetFixture.HostMode, physics: int = NetEntity.PHYSICS_SIMULATED) -> Dictionary:
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers("World", func(m: NetManager) -> void: m.physics = physics)
	var branches: Array[Node] = [net.server]
	branches.append_array(net.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: MODES.layer(n, bit))
	return managers


func _add(world: Node, scene: PackedScene, node_name: String, at: Vector3) -> Node3D:
	var n: Node3D = scene.instantiate()
	n.name = node_name
	n.position = at
	world.add_child(n)
	return n


func _copy(branch: Node, path: String) -> Node:
	assert_true(await net.wait_until(func() -> bool: return branch.has_node(path)), "%s reached %s" % [path, branch.name])
	return branch.get_node(path)


func _rested(bodies: Array) -> bool:
	return bodies.all(func(b: RigidBody3D) -> bool: return b.sleeping)


func test_owner_vehicle_moves_on_the_tick_of_input_and_converges_in_3d() -> void:
	timeout_sec = 20.0
	var managers := await _start(2, NetFixture.HostMode.HOST_PLAYER, NetEntity.PHYSICS_SERVER)
	var a: Node = net.clients[0]
	var b: Node = net.clients[1]
	var sv: RigidBody3D = VEHICLE.instantiate()
	sv.name = "V"
	sv.position = Vector3(0, 1, 0)
	var ne := NetEntity.new()
	ne.name = "NetEntity"
	ne.inherit_profile = true
	ne.owner_peer = net.peer_id(a)
	ne.physics = NetEntity.PHYSICS_OWNER
	sv.add_child(ne)
	managers["Server"].add_child(sv)
	var va: RigidBody3D = await _copy(a, "World/V")
	var vb: RigidBody3D = await _copy(b, "World/V")
	await _frames(20)
	assert_true(not va.freeze and vb.freeze and sv.freeze, "only the owner's body is live")
	var start := va.position
	Input.action_press("right")
	await get_tree().physics_frame
	assert_gt(va.linear_velocity.x, 0.0, "the owner's body moves on the tick of the input")
	var lat := -1
	for t in 60:
		if lat < 0 and sv.position.x > start.x + 0.005:
			lat = t
		await get_tree().physics_frame
	Input.action_release("right")
	assert_true(await net.wait_until(func() -> bool:
		return va.position.distance_to(sv.position) < SETTLED * 2.0 and va.position.distance_to(vb.position) < SETTLED * 2.0 and va.linear_velocity.length() < 0.01, 8.0),
		"the server and the other client converge on the owner (owner %s, server %s, other %s)" % [va.position, sv.position, vb.position])
	print("3D owner mode: the server's copy moved %d ticks after the press; the owner drove %.2f m" % [lat, va.position.x - start.x])
	assert_gt(va.position.x - start.x, 1.0, "the owner drove")


func test_simulated_push_is_corrected_without_a_jump_and_a_teleport_snaps_in_3d() -> void:
	timeout_sec = 20.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var cs := net.session(c)
	var sb: RigidBody3D = _add(managers["Server"], BOX, "B", Vector3(0, 0.25, 0))
	var cb: RigidBody3D = await _copy(c, "World/B")
	assert_true(await net.wait_until(func() -> bool: return sb.sleeping and cb.sleeping, 5.0), "both copies settle")
	assert_false(cb.freeze, "the client's copy is live")
	var before: Dictionary = cs.stats()
	sb.apply_central_impulse(Vector3(3.0, 0, 0) * sb.mass)
	var largest := 0.0
	var server_largest := 0.0
	var worst := 0.0
	var last := cb.position
	var slast := sb.position
	for t in 120:
		await get_tree().physics_frame
		largest = maxf(largest, cb.position.distance_to(last))
		server_largest = maxf(server_largest, sb.position.distance_to(slast))
		worst = maxf(worst, cb.position.distance_to(sb.position))
		last = cb.position
		slast = sb.position
	assert_true(await net.wait_until(func() -> bool: return sb.sleeping and cb.sleeping, 6.0), "both rest again")
	var err := cb.position.distance_to(sb.position)
	var after: Dictionary = cs.stats()
	print("3D simulated push: the server's box slid %.2f m; largest step %.3f m on the client, %.3f on the server; worst gap %.2f m; %d corrections; settled %.4f m apart" % [sb.position.x, largest, server_largest, worst, int(after["sim_corrections"]) - int(before["sim_corrections"]), err])
	assert_lt(largest, server_largest + 0.25 * worst, "no jump: beyond the server's own motion, a step covers less than a quarter of the gap")
	assert_lt(err, THRESHOLD, "converged to within the threshold")
	# A teleport (the physics server's transform, as a game moves a body) snaps.
	var target := sb.position + Vector3(-3.0, 0, 0)
	sb.sleeping = false
	var tr := sb.global_transform
	tr.origin = target
	PhysicsServer3D.body_set_state(sb.get_rid(), PhysicsServer3D.BODY_STATE_TRANSFORM, tr)
	sb.global_position = target
	Net.touch(sb)
	assert_true(await net.wait_until(func() -> bool: return cb.position.distance_to(target) < 0.2, 3.0), "the client's copy jumped there (%s, server %s)" % [cb.position, sb.position])
	assert_eq(int(cs.stats()["sim_snaps"]) - int(after["sim_snaps"]), 1, "one snap")


func test_simulated_resting_pile_stays_asleep_and_converges_in_3d() -> void:
	timeout_sec = 25.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var cs := net.session(c)
	var boxes: Array = []
	# Four layers of eight boxes, each where it rests, 5 cm apart.
	for i in PILE:
		@warning_ignore("integer_division")
		var layer := i / 8
		@warning_ignore("integer_division")
		var row := (i % 8) / 4
		boxes.append(_add(managers["Server"], BOX, "B%d" % i, Vector3((i % 4) * 0.55 - 0.8, 0.25 + layer * 0.5, row * 0.55)))
	assert_true(await net.wait_until(func() -> bool: return cs.entity_count() == PILE), "every box reached the client")
	var replicas: Array = boxes.map(func(s: Node) -> Node: return c.get_node("World/" + String(s.name)))
	assert_true(await net.wait_until(func() -> bool: return _rested(boxes) and _rested(replicas), 10.0), "both piles settle")
	await _frames(30)
	var before: Dictionary = cs.stats()
	await _frames(Engine.physics_ticks_per_second * 2)
	var after: Dictionary = cs.stats()
	var per_s := float(int(after["sim_corrections"]) - int(before["sim_corrections"])) / 2.0
	var worst := 0.0
	for i in PILE:
		worst = maxf(worst, (boxes[i] as Node3D).position.distance_to((replicas[i] as Node3D).position))
	var asleep := replicas.filter(func(b: RigidBody3D) -> bool: return b.sleeping).size()
	print("3D simulated pile: %d boxes; corrections while settling %d, at rest %.1f per second; %d asleep; worst gap %.4f m" % [PILE, int(before["sim_corrections"]), per_s, asleep, worst])
	assert_lt(per_s, 0.5, "a pile at rest is not corrected")
	assert_eq(asleep, PILE, "every replica stays asleep")
	assert_lt(worst, THRESHOLD, "converged to within the correction threshold")


## How far a replica may rest from the server's body once settled: half a `mm` step on each axis.
const SETTLED_EXACT := 0.0009


func _worst(server: Array, client: Array) -> float:
	var worst := 0.0
	for i in server.size():
		worst = maxf(worst, (server[i] as Node3D).position.distance_to((client[i] as Node3D).position))
	return worst


func test_a_replica_resting_just_beyond_the_threshold_settles_onto_the_server_pose_and_sleeps_in_3d() -> void:
	# The measured defect: a replica resting 0.0201 m from the server's body, just over the 0.02 m
	# threshold, was corrected 3 to 4 times a second, each correction woke it, and it never slept.
	timeout_sec = 20.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var sb: RigidBody3D = _add(managers["Server"], BOX, "Box", Vector3(0.0, 0.25, 0.0))
	var cb: RigidBody3D = await _copy(c, "World/Box")
	assert_true(await net.wait_until(func() -> bool: return sb.sleeping and cb.sleeping and cb.position.distance_to(sb.position) < SETTLED_EXACT, 5.0), "both rest, together")
	# The client's box is put 0.021 m off, where it rests.
	var t := cb.global_transform
	t.origin.x += 0.021
	cb.global_transform = t
	PhysicsServer3D.body_set_state(cb.get_rid(), PhysicsServer3D.BODY_STATE_TRANSFORM, t)
	var c0: int = net.session(c).stats()["sim_corrections"]
	var f0 := Engine.get_physics_frames()
	assert_true(await net.wait_until(func() -> bool: return cb.sleeping and not cb.freeze and cb.position.distance_to(sb.position) < SETTLED_EXACT, 3.0), "settled onto the server's pose and asleep (%.4f m off)" % cb.position.distance_to(sb.position))
	var secs := float(Engine.get_physics_frames() - f0) / Engine.physics_ticks_per_second
	await _frames(Engine.physics_ticks_per_second)
	print("3D replica 0.021 m off at rest: settled onto the server's pose and asleep after %.2f s; %d corrections; %.5f m off a second later" % [secs, int(net.session(c).stats()["sim_corrections"]) - c0, cb.position.distance_to(sb.position)])
	# The quiet time (0.15 s) plus the stillness window (0.15 s) plus a short carry and a tick's wait.
	assert_lt(secs, 0.5, "within half a second")
	assert_eq(int(net.session(c).stats()["sim_corrections"]) - c0, 0, "without a correction (nothing woken)")
	assert_true(cb.sleeping, "and it stays asleep")
	assert_lt(cb.position.distance_to(sb.position), SETTLED_EXACT, "where the server's is")


func test_a_pile_that_settles_while_a_client_watches_sleeps_there_soon_after_the_server_in_3d() -> void:
	# Reported: a pile that settled while a client watched fell asleep there about 10 s after the
	# server's (about 1 s after it was dropped).
	timeout_sec = 25.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var boxes: Array = []
	# Two layers of eight boxes, dropped from 0.3 m above where they rest, a little turned.
	for i in 16:
		@warning_ignore("integer_division")
		var layer := i / 8
		@warning_ignore("integer_division")
		var row := (i % 8) / 4
		var b: RigidBody3D = _add(managers["Server"], BOX, "B%d" % i, Vector3((i % 4) * 0.55 - 0.8, 0.55 + layer * 0.6, row * 0.55))
		b.rotation = Vector3(0.0, 0.1 * i, 0.05)
		boxes.append(b)
	var replicas: Array = []
	for b: Node in boxes:
		replicas.append(await _copy(c, "World/" + String(b.name)))
	assert_true(await net.wait_until(func() -> bool: return _rested(boxes), 8.0), "the server's pile sleeps")
	var f0 := Engine.get_physics_frames()
	assert_true(await net.wait_until(func() -> bool: return _rested(replicas) and _worst(boxes, replicas) < SETTLED_EXACT, 8.0), "the client's pile sleeps on the server's poses (%d awake, %.4f m)" % [replicas.filter(func(b: RigidBody3D) -> bool: return not b.sleeping).size(), _worst(boxes, replicas)])
	var secs := float(Engine.get_physics_frames() - f0) / Engine.physics_ticks_per_second
	await _frames(Engine.physics_ticks_per_second)
	print("3D pile settling while a client watches: the client's pile asleep on the server's poses %.2f s after the server's; worst gap a second later %.5f m; %d settles" % [secs, _worst(boxes, replicas), int(net.session(c).stats()["sim_settles"])])
	# The server's snapshots stop when its pile sleeps; the client needs the quiet time (0.15 s), the
	# stillness window (0.15 s) and the carry (at most 0.25 s), after the newest snapshot reached it
	# (half a round trip plus the jitter buffer, about 0.1 s here): about 0.7 s (measured: 0.8 s). A
	# body a neighbour's settle knocked off settles again after the retry time (0.5 s): 1.5 s.
	assert_lt(secs, 1.5, "within a second and a half of the server's")
	assert_true(_rested(replicas), "and it stays asleep")
	assert_lt(_worst(boxes, replicas), SETTLED_EXACT, "on the server's poses")


func _platform_rollbacks(physics: int) -> int:
	var managers := await _start(1, NetFixture.HostMode.DEDICATED, physics)
	var c: Node = net.clients[0]
	var cs := net.session(c)
	var platform: RigidBody3D = _add(managers["Server"], PLATFORM, "Platform", Vector3(0, 1.0, 0))
	platform.linear_velocity = Vector3(0.4, 0, 0)
	var sp: Node = Net.spawn_for(net.peer_id(c), PLAYER, managers["Server"], "P")
	(sp as Node3D).position = Vector3(0, 1.2, 0)
	var cp: CharacterBody3D = await _copy(c, "World/P")
	assert_true(await net.wait_until(func() -> bool: return cp.is_on_floor(), 5.0), "the player stands on the platform")
	await _frames(30)
	var r0: int = cs.stats()["rollbacks"]
	await _frames(180)
	var rolled: int = int(cs.stats()["rollbacks"]) - r0
	net.teardown()
	net = null
	return rolled


func test_a_predicted_player_on_a_swinging_simulated_platform_does_not_roll_back_every_tick_in_3d() -> void:
	timeout_sec = 25.0
	var sim := await _platform_rollbacks(NetEntity.PHYSICS_SIMULATED)
	var srv := await _platform_rollbacks(NetEntity.PHYSICS_SERVER)
	print("3D platform swinging at up to 0.4 m/s, 100 ms round trip: %d rollbacks in 180 ticks on a simulated platform (%d on a server-mode one)" % [sim, srv])
	assert_lt(sim, 10, "on a simulated platform: fewer than one rollback in 18 ticks")
	assert_gt(srv, sim * 3, "where a frozen, interpolated one rolls the prediction back again and again")
