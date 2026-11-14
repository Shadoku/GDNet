extends TestCase
## WP18 3D parity (docs/PLAN_NETCODE.md 12.5, gate G10): the 3D twins of the physics families: a pile
## of server-owned `RigidBody3D` boxes that settles, converges and costs nothing asleep (WP14), and
## interaction scoring in metres with a `CharacterBody3D`'s slide collisions and a `RigidBody3D`'s
## contacts (WP14), in both host modes (G2). The branches share one physics space, so the clients'
## copies of bodies are moved to a collision layer of their own as they enter the tree.

const PLAYER := preload("res://tests/fixtures/net3d/player_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const BODY := preload("res://tests/fixtures/net3d/box_body_3d.tscn")
const CONTACT := preload("res://tests/fixtures/net3d/contact_player_3d.tscn")
## Half a `mm` step on three axes (0.0005 x sqrt(3)): the most a replica's position differs.
const HALF_STEP := 0.00087
## The most a `smallest3` round trip turns a rotation (radians; derived in test_net_3d.gd).
const ANGLE_BOUND := 1.5e-4
## The pile's size.
const PILE := 40

var net: NetFixture


func after_each() -> void:
	Input.action_release("right")
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _angle(a: Quaternion, b: Quaternion) -> float:
	var d := a.inverse() * b
	return 2.0 * asin(minf(1.0, Vector3(d.x, d.y, d.z).length()))


## Managers in every branch; on the clients, every RigidBody3D that enters the manager moves to layer 5
## and collides with nothing (the server's bodies and the floor are on layer 1). Players need no shift:
## their layer is 0 and their mask the floor's.
func _managers(f: NetFixture) -> Dictionary:
	var managers := f.add_managers()
	for c in f.clients:
		(managers[String(c.name)] as Node).child_entered_tree.connect(func(n: Node) -> void:
			if n is RigidBody3D:
				(n as RigidBody3D).collision_layer = 1 << 4
				(n as RigidBody3D).collision_mask = 0)
	return managers


func _body(world: Node, body_name: String, at: Vector3, scene: PackedScene = BODY) -> RigidBody3D:
	var b: RigidBody3D = scene.instantiate()
	b.name = body_name
	b.position = at
	world.add_child(b)
	return b


# --- bodies (WP14) --------------------------------------------------------------------------------


func _bodies(branch: Node) -> Array[RigidBody3D]:
	var out: Array[RigidBody3D] = []
	for n in branch.get_node("World").get_children():
		if n is RigidBody3D:
			out.append(n)
	return out


func _max_error(f: NetFixture) -> Vector2:
	var worst := Vector2.ZERO
	var c: Node = f.clients[0]
	for b in _bodies(f.server):
		var r := c.get_node_or_null("World/" + String(b.name)) as RigidBody3D
		if r == null:
			return Vector2(INF, INF)
		worst.x = maxf(worst.x, b.position.distance_to(r.position))
		worst.y = maxf(worst.y, _angle(b.quaternion, r.quaternion))
	return worst


func test_bodies_a_pile_of_rigid_bodies_settles_converges_and_costs_nothing_asleep() -> void:
	timeout_sec = 30.0
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var world: Node = _managers(f)["Server"]
		var c: Node = f.clients[0]
		var rng := RandomNumberGenerator.new()
		rng.seed = 18
		# Two layers of 20 boxes, a little turned, dropped from 0.4 and 1.0 m.
		var boxes: Array[RigidBody3D] = []
		for i in PILE:
			var b := _body(world, "B%d" % i, Vector3((i % 5) * 0.6 - 1.2 + rng.randf_range(-0.1, 0.1), 0.4 + 0.6 * (i / 20), ((i / 5) % 4) * 0.6 - 0.9))
			b.rotate_y(rng.randf_range(-0.5, 0.5))
			b.rotate_x(rng.randf_range(-0.05, 0.05))
			boxes.append(b)
		assert_true(await f.wait_until(func() -> bool: return f.session(c).entity_count() == PILE), "every box reached the client")
		# A body update's size: snapshot bytes over the boxes carried, while they fall.
		var bytes0: int = f.server_session.stats()["snapshot_bytes"]
		var sends0 := 0
		var cid := f.peer_id(c)
		for b in boxes:
			sends0 += f.server_session.entity_sends(b, cid)
		await _frames(15)
		var sends := -sends0
		for b in boxes:
			sends += f.server_session.entity_sends(b, cid)
		var update_bytes := float(int(f.server_session.stats()["snapshot_bytes"]) - bytes0) / maxf(1.0, sends)
		var t0 := Time.get_ticks_msec()
		var still := [0]
		assert_true(await f.wait_until(func() -> bool:
			var asleep := boxes.all(func(b: RigidBody3D) -> bool: return b.sleeping)
			still[0] = still[0] + 1 if asleep else 0
			return still[0] >= 30, 10.0), "the pile settles: every box sleeps")
		var settled := (Time.get_ticks_msec() - t0) / 1000.0
		assert_true(await f.wait_until(func() -> bool:
			var e := _max_error(f)
			return e.x < HALF_STEP and e.y < ANGLE_BOUND, 3.0), "the client converged: %s" % _max_error(f))
		var err := _max_error(f)
		for r in _bodies(c):
			assert_true(r.freeze, "replica %s is frozen" % r.name)
		await _frames(5)
		var before: Dictionary = f.server_session.stats()
		await _frames(Engine.physics_ticks_per_second + 2)
		var after: Dictionary = f.server_session.stats()
		var per_s: PackedInt64Array = after["bytes_sent_per_s"]
		print("3D pile (mode %d, %s): %d boxes asleep %.1f s after the fall started; a body update %.1f bytes (%d updates); converged to %.6f m and %.1f microrad; asleep %d B/s (%d snapshots)" % [f.host_mode, ProjectSettings.get_setting("physics/3d/physics_engine"), PILE, settled, update_bytes, sends, err.x, err.y * 1e6, per_s[0] + per_s[1] + per_s[2], per_s[1]])
		assert_gt(sends, PILE, "falling boxes were sent")
		assert_eq(int(after["changes"]) - int(before["changes"]), 0, "no sleeping box recorded a change")
		assert_eq(int(after["snapshot_bytes"]) - int(before["snapshot_bytes"]), 0, "no snapshot bytes for a sleeping pile")
		assert_eq(f.session(c).stats()["divergences"], 0, "hash telemetry saw no divergence"))
	assert_true(ok)


# --- interaction scoring (WP14) -------------------------------------------------------------------


func test_interaction_radius_in_metres_slide_collisions_and_rigid_body_contacts() -> void:
	timeout_sec = 20.0
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var world: NetManager = _managers(f)["Server"]
		var cid := f.peer_id(f.clients[0])
		var s := f.server_session
		# The player stands beside box C; A is 2.5 m from it and B 3.6 m: the 300 px setting is 3 m in 3D.
		var beside := _body(world, "C", Vector3(8, 0.25, 0))
		var near := _body(world, "A", Vector3(7.5, 0.25, 2.5))
		var far := _body(world, "B", Vector3(7.5, 0.25, -3.6))
		var p: CharacterBody3D = Net.spawn_for(cid, PLAYER, world, "P")
		p.position = Vector3(7.4, 0, 0)
		var id := func(n: Node) -> int: return Net.net_id(n)
		assert_true(await f.wait_until(func() -> bool: return p.is_on_floor() and Array(s.interacting(cid)).has(id.call(near)), 3.0), "the near box interacts")
		await _frames(5)
		var scored := Array(s.interacting(cid))
		assert_has(scored, id.call(beside), "the box beside the player")
		assert_false(scored.has(id.call(far)), "3.6 m is beyond 3 m: %s" % [scored])
		# Proximity down to 10 cm: the box the player walks into (more than 50 cm between the roots)
		# interacts through the player's slide collisions. (Standing still, GodotPhysics3D reports the
		# floor or box it rests on as a slide collision and Jolt reports none: guide 10.)
		world.interaction_radius = 0.1
		Input.action_press("right")
		assert_true(await f.wait_until(func() -> bool: return p.position.x > 7.49, 2.0), "the player reached the box")
		await _frames(3)
		scored = Array(s.interacting(cid))
		Input.action_release("right")
		assert_eq(scored, [id.call(beside)], "only the box the CharacterBody3D touches")
		# A RigidBody3D player with contact_monitor (a lid resting on box D): the bodies it touches.
		var lid := _body(world, "Lid", Vector3(-5, 0.75, 0), CONTACT)
		var rest := _body(world, "D", Vector3(-5, 0.25, 0))
		assert_true(s.set_player(cid, lid))
		assert_true(await f.wait_until(func() -> bool: return Array(s.interacting(cid)).has(id.call(rest)), 3.0), "the box the lid rests on interacts: %s" % [Array(s.interacting(cid))])
		# Asleep, the engine keeps reporting the contact.
		assert_true(await f.wait_until(func() -> bool: return lid.sleeping, 4.0), "the lid falls asleep")
		await _frames(10)
		scored = Array(s.interacting(cid))
		print("3D interaction (mode %d, %s): lid asleep %s, interacting %s" % [f.host_mode, ProjectSettings.get_setting("physics/3d/physics_engine"), lid.sleeping, scored])
		assert_eq(scored, [id.call(rest)], "still that box asleep, and nothing else"))
	assert_true(ok)
