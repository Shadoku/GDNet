extends TestCase
## WP19 slice A (docs/PLAN_NETCODE.md 12.4): the `simulated` mode in 2D. Every peer simulates the body;
## the client's replica is live and the server's snapshots correct it: a push it did not see slides it
## to the server's answer without a jump, a teleport snaps, a resting pile stays asleep (the plan's
## risk: corrections per second are counted and bounded), a predicted player on a moving platform does
## not roll back every tick, and a predicted player pushing a crate moves it in the tick of contact.
## Last, the modes mix in one scene with no script change.
##
## The branches share one physics space: every body entering a branch's manager goes to that branch's
## collision layer (bit 1 + branch index; the floor is layer 1), so the copies, all live, never touch.

const CRATE := preload("res://tests/fixtures/physics/crate.tscn")
const FLOOR := preload("res://tests/fixtures/physics/floor.tscn")
const VEHICLE := preload("res://tests/fixtures/physics/vehicle.tscn")
const PLATFORM := preload("res://tests/fixtures/physics/platform.tscn")
const WALKER := preload("res://tests/fixtures/physics/walker.tscn")
const MODES := preload("res://tests/integration/test_net_physics_modes.gd")
## The default position threshold: 0.02 m at 100 px per metre. Below it nothing is corrected, so a
## settled replica may differ from the server's by up to this much (far more than the 0.05 px a
## quantiser step would allow: the client's own physics put it there).
const THRESHOLD := 2.0
## The pile's size.
const PILE := 40

var net: NetFixture


func after_each() -> void:
	Input.action_release("right")
	ProjectSettings.set_setting("gdnet/physics/mode", null)
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


func _managers(f: NetFixture, physics: int = NetEntity.PHYSICS_SIMULATED) -> Dictionary:
	var managers := f.add_managers("World", func(m: NetManager) -> void: m.physics = physics)
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: MODES.layer(n, bit))
	return managers


func _start(clients: int, mode: NetFixture.HostMode, latency_ms: float = 50.0) -> Dictionary:
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(latency_ms))
	net.start()
	assert_true(await net.wait_connected())
	return _managers(net)


func _add(world: Node, scene: PackedScene, node_name: String, at: Vector2) -> Node:
	var n: Node2D = scene.instantiate()
	n.name = node_name
	n.position = at
	world.add_child(n)
	return n


func _copy(branch: Node, path: String) -> Node:
	assert_true(await net.wait_until(func() -> bool: return branch.has_node(path)), "%s reached %s" % [path, branch.name])
	return branch.get_node(path)


## Moves a RigidBody2D the way a game teleports one (its own setters do not reach the physics server
## while it simulates), and tells gdnet a sleeping body changed (WP18: it is not polled asleep).
func _teleport(b: RigidBody2D, at: Vector2) -> void:
	b.sleeping = false
	var t := b.global_transform
	t.origin = at
	PhysicsServer2D.body_set_state(b.get_rid(), PhysicsServer2D.BODY_STATE_TRANSFORM, t)
	b.global_position = at
	Net.touch(b)


func _rested(bodies: Array) -> bool:
	return bodies.all(func(b: RigidBody2D) -> bool: return b.sleeping)


# --- corrections ------------------------------------------------------------------------------------


func test_a_push_on_the_server_is_corrected_without_a_jump_and_a_teleport_snaps() -> void:
	timeout_sec = 25.0
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var managers := await _start(1, mode)
		var c: Node = net.clients[0]
		var cs := net.session(c)
		var sc: RigidBody2D = _add(managers["Server"], CRATE, "Crate", Vector2(0, 280))
		var cc: RigidBody2D = await _copy(c, "World/Crate")
		assert_true(await net.wait_until(func() -> bool: return sc.sleeping and cc.sleeping, 5.0), "both copies settle")
		assert_false(cc.freeze, "the client's copy is a live body")
		assert_lt(absf(cc.position.x - sc.position.x), THRESHOLD, "the two piles of one agree")
		var before: Dictionary = cs.stats()
		var x0 := sc.position.x
		# A push only the server's copy gets.
		sc.apply_central_impulse(Vector2(300, 0))
		var largest := 0.0
		var server_largest := 0.0
		var last := cc.position
		var slast := sc.position
		var worst := 0.0
		for t in 150:
			await get_tree().physics_frame
			largest = maxf(largest, cc.position.distance_to(last))
			server_largest = maxf(server_largest, sc.position.distance_to(slast))
			worst = maxf(worst, cc.position.distance_to(sc.position))
			last = cc.position
			slast = sc.position
		assert_true(await net.wait_until(func() -> bool: return sc.sleeping and cc.sleeping, 5.0), "both rest again")
		var err := cc.position.distance_to(sc.position)
		var after: Dictionary = cs.stats()
		var corrections := int(after["sim_corrections"]) - int(before["sim_corrections"])
		print("simulated push (mode %d): the server moved %.1f px; largest step %.2f px on the client, %.2f on the server; worst gap %.1f px; %d corrections; settled %.2f px apart" % [mode, absf(sc.position.x - x0), largest, server_largest, worst, corrections, err])
		assert_gt(corrections, 0, "the client was corrected")
		assert_lt(largest, server_largest + 0.25 * worst, "no jump: beyond the server's own motion, a step covers less than a quarter of the gap")
		assert_lt(err, THRESHOLD, "converged to within the threshold")
		assert_eq(int(after["sim_snaps"]) - int(before["sim_snaps"]), 0, "no snap for a push")
		# A teleport snaps.
		var target := Vector2(sc.position.x - 250.0 if sc.position.x > 100.0 else sc.position.x + 250.0, sc.position.y)
		_teleport(sc, target)
		assert_true(await net.wait_until(func() -> bool: return cc.position.distance_to(target) < 20.0, 3.0), "the client's copy jumped there (%s, server %s)" % [cc.position, sc.position])
		assert_eq(int(cs.stats()["sim_snaps"]) - int(after["sim_snaps"]), 1, "one snap")
		assert_eq(int(net.server_session.stats()["sim_corrections"]), 0, "the server never corrects its own bodies")
		net.teardown()
		net = null


func test_a_resting_pile_stays_asleep_and_converges() -> void:
	timeout_sec = 20.0
	# The pile settles on the server first; the client joins it at rest (its burst creates live
	# replicas where the server's crates rest).
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.set_emulation(net.clients[0], _link(30.0))
	net.start_server()
	var managers := _managers(net)
	var c: Node = net.clients[0]
	var cs := net.session(c)
	var world: Node = managers["Server"]
	var crates: Array = []
	# Ten columns of four, each crate where it rests (the floor's top is y = 300), 2 px apart.
	for i in PILE:
		@warning_ignore("integer_division")
		var row := i / 10
		crates.append(_add(world, CRATE, "C%d" % i, Vector2(-80 + (i % 10) * 22, 290 - row * 20)))
	assert_true(await net.wait_until(func() -> bool: return _rested(crates), 5.0), "the server's pile settles")
	net.start_clients()
	assert_true(await net.wait_connected())
	assert_true(await net.wait_until(func() -> bool: return cs.entity_count() == PILE), "every crate reached the client")
	var replicas: Array = crates.map(func(s: Node) -> Node: return c.get_node("World/" + String(s.name)))
	var t_settle := Time.get_ticks_msec()
	assert_true(await net.wait_until(func() -> bool: return _rested(replicas), 5.0), "the client's pile falls asleep (%d awake)" % replicas.filter(func(b: RigidBody2D) -> bool: return not b.sleeping).size())
	t_settle = Time.get_ticks_msec() - t_settle
	await _frames(30)
	var before: Dictionary = cs.stats()
	var bytes0: int = net.server_session.stats()["snapshot_bytes"]
	var seconds := 2
	await _frames(Engine.physics_ticks_per_second * seconds)
	var after: Dictionary = cs.stats()
	var per_s := float(int(after["sim_corrections"]) - int(before["sim_corrections"])) / seconds
	var worst := 0.0
	for i in PILE:
		worst = maxf(worst, (crates[i] as Node2D).position.distance_to((replicas[i] as Node2D).position))
	var asleep := replicas.filter(func(b: RigidBody2D) -> bool: return b.sleeping).size()
	print("simulated pile: %d crates; the client's asleep %.1f s after it had them; corrections until then %d, at rest %.1f per second; %d of %d replicas asleep; worst gap %.2f px; snapshot bytes at rest %d" % [PILE, t_settle / 1000.0, int(before["sim_corrections"]), per_s, asleep, PILE, worst, int(net.server_session.stats()["snapshot_bytes"]) - bytes0])
	assert_lt(per_s, 0.5, "a pile at rest is not corrected (fewer than one correction every two seconds)")
	assert_eq(asleep, PILE, "every replica stays asleep")
	assert_lt(worst, THRESHOLD, "the replicas converged to within the correction threshold")
	assert_eq(int(net.server_session.stats()["snapshot_bytes"]) - bytes0, 0, "a resting pile costs no snapshot bytes")


# --- predicted players ------------------------------------------------------------------------------


## A walker owned by client A standing on a platform that swings on a spring (40 px/s through the
## middle, a period of three seconds); returns the rollbacks over
## `seconds` once it stands, and its server copy's gap to the owner's.
func _platform_rollbacks(physics: int, seconds: float) -> Dictionary:
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var cs := net.session(c)
	var platform: RigidBody2D = _add(managers["Server"], PLATFORM, "Platform", Vector2(0, 150))
	Net.set_physics(platform, physics)
	platform.linear_velocity = Vector2(40, 0)
	var sw: CharacterBody2D = Net.spawn_for(net.peer_id(c), WALKER, managers["Server"], "W")
	sw.position = Vector2(0, 120)
	var cw: CharacterBody2D = await _copy(c, "World/W")
	var cp: RigidBody2D = await _copy(c, "World/Platform")
	assert_true(await net.wait_until(func() -> bool: return cw.is_on_floor() and sw.is_on_floor(), 5.0), "the walker stands on the platform")
	await _frames(30)
	var r0: int = cs.stats()["rollbacks"]
	var frames := int(seconds * Engine.physics_ticks_per_second)
	var worst := 0.0
	for t in frames:
		await get_tree().physics_frame
		worst = maxf(worst, absf(cw.position.x - cp.position.x - (sw.position.x - platform.position.x)))
	var rolled: int = int(cs.stats()["rollbacks"]) - r0
	if physics == NetEntity.PHYSICS_SIMULATED:
		assert_true(cw.is_on_floor(), "still on the platform")
	var out := {"rollbacks": rolled, "frames": frames, "worst": worst, "travel": cp.position.x}
	net.teardown()
	net = null
	return out


func test_a_predicted_player_on_a_moving_simulated_platform_does_not_roll_back_every_tick() -> void:
	timeout_sec = 25.0
	var sim := await _platform_rollbacks(NetEntity.PHYSICS_SIMULATED, 3.0)
	var srv := await _platform_rollbacks(NetEntity.PHYSICS_SERVER, 3.0)
	print("platform swinging at up to 40 px/s, 100 ms round trip: %d rollbacks in %d ticks on a simulated platform (%d on a server-mode one)" % [sim["rollbacks"], sim["frames"], srv["rollbacks"]])
	assert_lt(sim["rollbacks"], 10, "on a simulated platform: fewer than one rollback in 18 ticks")
	assert_gt(srv["rollbacks"], sim["rollbacks"] * 3, "where a frozen, interpolated one rolls the prediction back again and again")


func test_a_predicted_player_pushing_a_simulated_crate_moves_it_in_the_tick_of_contact() -> void:
	timeout_sec = 20.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	var sc: RigidBody2D = _add(managers["Server"], CRATE, "Crate", Vector2(60, 290))
	var sw: CharacterBody2D = Net.spawn_for(net.peer_id(c), WALKER, managers["Server"], "W")
	sw.position = Vector2(0, 288)
	var cw: CharacterBody2D = await _copy(c, "World/W")
	var cc: RigidBody2D = await _copy(c, "World/Crate")
	assert_true(await net.wait_until(func() -> bool: return cw.is_on_floor() and cc.sleeping and sc.sleeping, 5.0), "everything rests")
	var x0 := cc.position.x
	Input.action_press("right")
	assert_true(await net.wait_until(func() -> bool: return cw.first_contact_frame >= 0, 3.0), "the walker reached the crate")
	# The walker's tick of contact has run, and the physics step after it.
	var contact_frame: int = cw.first_contact_frame
	var moved_by := cc.position.x - x0
	var frames_after := Engine.get_physics_frames() - contact_frame
	await _frames(40)
	Input.action_release("right")
	assert_true(await net.wait_until(func() -> bool: return sc.sleeping and cc.sleeping, 6.0), "the crate rests on both")
	print("simulated push by a predicted player: the client's crate moved %.2f px %d physics frame(s) after the contact; settled %.2f px from the server's; %d rollbacks" % [moved_by, frames_after, cc.position.distance_to(sc.position), net.session(c).stats()["rollbacks"]])
	assert_gt(moved_by, 0.0, "the crate moved in the tick of the contact on the client")
	assert_lt(frames_after, 2, "measured on the very next frame")
	assert_gt(sc.position.x, 61.0, "the server's crate was pushed too")
	assert_lt(cc.position.distance_to(sc.position), THRESHOLD, "and they agree")


# --- mixed ------------------------------------------------------------------------------------------


func test_modes_mix_in_one_scene_with_no_script_change() -> void:
	timeout_sec = 20.0
	var managers := await _start(2, NetFixture.HostMode.HOST_PLAYER)
	var a: Node = net.clients[0]
	var world: Node = managers["Server"]
	# A `server` pile (the configure key), an `owner` vehicle (the property) and `simulated` crates
	# (the manager's default): the same script-less scenes and the same vehicle script.
	var pile: Array = []
	for i in 4:
		var p: RigidBody2D = _add(world, CRATE, "P%d" % i, Vector2(160 + i * 24, 285))
		Net.configure(p, {"physics": "server"})
		pile.append(p)
	var crates: Array = []
	for i in 3:
		crates.append(_add(world, CRATE, "S%d" % i, Vector2(-40 + i * 30, 285)))
	var v: RigidBody2D = VEHICLE.instantiate()
	v.name = "V"
	v.position = Vector2(0, 100)
	var ne := NetEntity.new()
	ne.name = "NetEntity"
	ne.inherit_profile = true
	ne.owner_peer = net.peer_id(a)
	ne.physics = NetEntity.PHYSICS_OWNER
	v.add_child(ne)
	world.add_child(v)
	for branch in net.clients:
		for n in ["P0", "P1", "P2", "P3", "S0", "S1", "S2", "V"]:
			await _copy(branch, "World/" + n)
	await _frames(5)
	for branch in net.clients:
		for i in 4:
			assert_true((branch.get_node("World/P%d" % i) as RigidBody2D).freeze, "%s: the server pile is frozen" % branch.name)
		for i in 3:
			assert_false((branch.get_node("World/S%d" % i) as RigidBody2D).freeze, "%s: simulated crates are live" % branch.name)
	assert_false((a.get_node("World/V") as RigidBody2D).freeze, "the owner drives its vehicle")
	assert_true((net.clients[1].get_node("World/V") as RigidBody2D).freeze, "the other client shows it")
	assert_true(v.freeze, "and so does the server")
	Input.action_press("right")
	await _frames(20)
	Input.action_release("right")
	var va: RigidBody2D = a.get_node("World/V")
	var off := [""]
	assert_true(await net.wait_until(func() -> bool:
		if absf(va.position.x - v.position.x) > 0.1 or va.linear_velocity.length() > 1.0:
			off[0] = "vehicle: owner %s, server %s" % [va.position, v.position]
			return false
		for branch in net.clients:
			for p: RigidBody2D in pile + crates:
				var r := branch.get_node("World/" + String(p.name)) as Node2D
				if r.position.distance_to(p.position) > THRESHOLD:
					off[0] = "%s on %s: %s, server %s" % [p.name, branch.name, r.position, p.position]
					return false
		return true, 8.0), "every body agrees on every peer (%s)" % off[0])
