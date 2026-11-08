extends TestCase
## WP3 (docs/PLAN_NETCODE.md 2.1 "Profiles" and "Dirty tracking", 2.4, 2.7, 5): state replication from
## inferred profiles. Gate G7 (a plain single-player scene replicates with no script edits), gate G4
## (bandwidth), configure(), NetEntity lists, late join, the profile check, loss, and the polling
## benchmark. Headless over NetPipePeer; the gate scenarios run in both host modes (G2).

const LEVEL := preload("res://tests/fixtures/zero_change/level.tscn")
const ZERO_CHANGE_DIR := "res://tests/fixtures/zero_change"

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _near(a: Vector2, b: Vector2, tol: float) -> bool:
	return a.distance_to(b) <= tol


## The ways the client's copy of the level may differ from the server's, as text (empty = matches).
func _level_diff(s: Node, c: Node) -> String:
	var out: PackedStringArray = []
	var sp: CharacterBody2D = s.get_node("Player")
	var cp: CharacterBody2D = c.get_node("Player")
	if not _near(sp.position, cp.position, 4.0):
		out.append("player position %s vs %s" % [sp.position, cp.position])
	# WP5: velocity is interpolated, so the replica shows it about a jitter buffer (plus a tick of
	# transit) later; the wanderer turns 1.2 px/s per tick, hence 6 (five ticks) instead of WP3's 2.
	if not _near(sp.velocity, cp.velocity, 6.0):
		out.append("player velocity %s vs %s" % [sp.velocity, cp.velocity])
	var ss: AnimatedSprite2D = s.get_node("Player/Sprite")
	var cs: AnimatedSprite2D = c.get_node("Player/Sprite")
	if ss.animation != cs.animation:
		out.append("animation %s vs %s" % [ss.animation, cs.animation])
	var n := ss.sprite_frames.get_frame_count(ss.animation)
	var df := absi(ss.frame - cs.frame)
	if mini(df, n - df) > 1:
		out.append("frame %d vs %d" % [ss.frame, cs.frame])
	var sa: AnimationPlayer = s.get_node("Platform/Anim")
	var ca: AnimationPlayer = c.get_node("Platform/Anim")
	if sa.current_animation != ca.current_animation:
		out.append("current_animation %s vs %s" % [sa.current_animation, ca.current_animation])
	if absf(sa.current_animation_position - ca.current_animation_position) > 0.25:
		out.append("animation position %f vs %f" % [sa.current_animation_position, ca.current_animation_position])
	if not _near(s.get_node("Platform").position, c.get_node("Platform").position, 12.0):
		out.append("platform %s vs %s" % [s.get_node("Platform").position, c.get_node("Platform").position])
	var st: Node = s.get_node("Stats")
	var ct: Node = c.get_node("Stats")
	if st.count != ct.count:
		out.append("count %d vs %d" % [st.count, ct.count])
	if absf(st.ratio - ct.ratio) > 0.1:
		out.append("ratio %f vs %f" % [st.ratio, ct.ratio])
	if st.title != ct.title:
		out.append("title %s vs %s" % [st.title, ct.title])
	var sl: Label = s.get_node("Score")
	var cl: Label = c.get_node("Score")
	if sl.text != cl.text:
		out.append("label %s vs %s" % [sl.text, cl.text])
	if not _near(s.get_node("Ball").position, c.get_node("Ball").position, 4.0):
		out.append("ball %s vs %s" % [s.get_node("Ball").position, c.get_node("Ball").position])
	return ", ".join(out)


func test_g7_the_zero_change_scene_has_no_net_code() -> void:
	var dir := DirAccess.open(ZERO_CHANGE_DIR)
	assert_not_null(dir)
	var scripts := 0
	for file in dir.get_files():
		if file.get_extension() in ["gd", "tscn"]:
			var text := FileAccess.get_file_as_string(ZERO_CHANGE_DIR.path_join(file))
			assert_false(text.contains("Net."), "%s mentions Net." % file)
			assert_false(text.contains("NetEntity"), "%s mentions NetEntity" % file)
			if file.get_extension() == "gd":
				scripts += 1
	assert_eq(scripts, 2, "the player and the stats scripts")


func test_g7_a_plain_scene_replicates_with_no_script_edits() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		# The same level on every peer, as level content under the manager. The fixture's branches share
		# one physics space (one process), so the test moves each client copy's bodies to their own
		# collision layer; the scene and its scripts are untouched.
		var made := [0]
		f.add_managers("World", func(m: NetManager) -> void:
			var level := LEVEL.instantiate()
			made[0] += 1
			if made[0] > 1:   # add_managers configures the server's manager first
				for body in level.find_children("*", "CollisionObject2D", true, false):
					(body as CollisionObject2D).collision_layer = 2
					(body as CollisionObject2D).collision_mask = 2
			m.add_child(level))
		var s: Node = f.server.get_node("World/Level")
		var c: Node = f.clients[0].get_node("World/Level")
		assert_true(await f.wait_until(func() -> bool: return Net.net_id(c) > 0), "the client bound its level")
		# Random starts on each side (velocity, counters) converge to the server's.
		var matched: bool = await f.wait_until(func() -> bool: return _level_diff(s, c) == "", 4.0)
		assert_true(matched, "the client matches the server: " + _level_diff(s, c))
		# Changes the client's own scripts never make.
		(s.get_node("Score") as Label).visible = false
		(s.get_node("Player/Sprite") as AnimatedSprite2D).play("idle")
		(s.get_node("Platform") as Node2D).modulate = Color(1, 0, 0)
		assert_true(await f.wait_until(func() -> bool:
			return not (c.get_node("Score") as Label).visible \
				and (c.get_node("Player/Sprite") as AnimatedSprite2D).animation == &"idle" \
				and (c.get_node("Platform") as Node2D).modulate.is_equal_approx(Color(1, 0, 0)), 2.0),
			"visible, animation and modulate propagated")
		assert_true((c.get_node("Ball") as RigidBody2D).freeze, "the RigidBody replica is frozen")
		assert_false((s.get_node("Ball") as RigidBody2D).freeze, "the server's body simulates")
		matched = await f.wait_until(func() -> bool: return _level_diff(s, c) == "", 4.0)
		assert_true(matched, "still matching: " + _level_diff(s, c)))
	assert_true(ok, "both fixtures connected")


const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const CRATE_SNAP := preload("res://tests/fixtures/net/crate_snap.tscn")
const CONFIGURED := preload("res://tests/fixtures/net/configured.tscn")
const LISTED := preload("res://tests/fixtures/net/listed.tscn")
const INHERITED := preload("res://tests/fixtures/net/inherited.tscn")
const BENCH := preload("res://tests/fixtures/net/bench.tscn")


func _crate(crate_name: String) -> Node2D:
	var c: Node2D = CRATE.instantiate()
	c.name = crate_name
	return c


func test_default_profiles_file_is_the_builtin_table() -> void:
	var shipped: NetProfiles = load("res://addons/gdnet/default_profiles.tres")
	assert_not_null(shipped)
	assert_eq(shipped.profile_hash(), NetProfiles.builtin().profile_hash(), "default_profiles.tres equals the built-in table")
	assert_true(shipped.classes.has("CharacterBody2D"))
	assert_eq((shipped.classes["Camera2D"] as Array).size(), 0, "cameras sync nothing")
	# merge(): an entry replaces by name, remove drops it, an empty list empties a class.
	var p := NetProfiles.builtin()
	var over := NetProfiles.new()
	over.classes = {"Label": [{"name": "text", "remove": true}, {"name": "uppercase"}], "Timer": [{"name": "wait_time"}]}
	p.merge(over)
	var label: Array = p.classes["Label"]
	assert_eq(label.size(), 1)
	assert_eq(label[0]["name"], "uppercase")
	assert_eq((p.classes["Timer"] as Array).size(), 1)


func test_the_initial_state_arrives_before_net_spawned() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var c := _crate("Placed")
		c.position = Vector2(123.5, -40.25)
		c.rotation = 1.25
		(c.get_node("Sprite") as Sprite2D).flip_h = true
		managers["Server"].add_child(c)
		var seen := {}
		f.session(client).entity_spawned.connect(func(n: Node) -> void:
			seen["position"] = (n as Node2D).position
			seen["flip"] = (n.get_node("Sprite") as Sprite2D).flip_h)
		assert_true(await f.wait_until(func() -> bool: return seen.has("position")))
		assert_true(_near(seen["position"], Vector2(123.5, -40.25), 0.001), "position at spawn: %s" % seen["position"])
		assert_true(seen["flip"], "a cold property at spawn too")
		assert_almost_eq((client.get_node("World/Placed") as Node2D).rotation, 1.25, 0.001))
	assert_true(ok)


func test_configure_adds_a_plain_var_and_removes_an_inferred_property() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var client: Node = net.clients[0]
	var node: Node2D = CONFIGURED.instantiate()
	# Every peer makes the same call before the scene's first spawn (here both branches share one
	# process, so one call configures the scene for both).
	Net.configure(node, {"state": ["+phase", "-rotation", "+Sprite:centered"], "quantize": {"position": "cm"}})
	managers["Server"].add_child(node)
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Configured")))
	var replica: Node2D = client.get_node("World/Configured")
	node.phase = 7
	node.hp = 33
	node.rotation = 2.0
	node.position = Vector2(10.004, 20.0)
	(node.get_node("Sprite") as Sprite2D).centered = false
	assert_true(await net.wait_until(func() -> bool: return replica.phase == 7 and replica.hp == 33 and not (replica.get_node("Sprite") as Sprite2D).centered), "the added var and the export travel")
	assert_true(await net.wait_until(func() -> bool: return _near(replica.position, Vector2(10.0, 20.0), 0.006)), "position at cm precision")
	await _frames(20)
	assert_eq(replica.rotation, 0.0, "the removed rotation never travels")
	# Too late now: the table of this scene is fixed.
	allow_errors = true
	Net.configure(node, {"state": ["+skew"]})


func test_a_net_entity_list_replaces_inference_and_inherit_unites() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var client: Node = net.clients[0]
	var listed: Node2D = LISTED.instantiate()
	var inherited: Node2D = INHERITED.instantiate()
	managers["Server"].add_child(listed)
	managers["Server"].add_child(inherited)
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Listed") and client.has_node("World/Inherited")))
	var rl: Node2D = client.get_node("World/Listed")
	var ri: Node2D = client.get_node("World/Inherited")
	listed.position = Vector2(5, 6)
	(listed.get_node("Sprite") as Sprite2D).frame = 2
	listed.rotation = 1.0
	listed.hp = 99
	inherited.position = Vector2(7, 8)
	inherited.rotation = 0.5
	inherited.phase = 4
	inherited.hp = 42
	(inherited.get_node("Sprite") as Sprite2D).centered = false
	assert_true(await net.wait_until(func() -> bool:
		return _near(rl.position, Vector2(5, 6), 0.01) and (rl.get_node("Sprite") as Sprite2D).frame == 2), "the listed properties travel")
	assert_true(await net.wait_until(func() -> bool:
		return _near(ri.position, Vector2(7, 8), 0.01) and absf(ri.rotation - 0.5) < 0.001 and ri.phase == 4 \
			and ri.hp == 42 and not (ri.get_node("Sprite") as Sprite2D).centered), "inherit_profile: inferred and listed properties travel")
	await _frames(20)
	assert_eq(rl.rotation, 0.0, "an unlisted property does not travel")
	assert_eq(rl.hp, 10, "not even an export")


func test_a_different_property_table_disconnects_naming_scene_and_property() -> void:
	net = NetFixture.new(self, 1)
	net.client_sessions[0]._debug_perturb_property = "rotation"
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	managers["Server"].add_child(_crate("Mismatch"))
	assert_true(await net.wait_until(func() -> bool: return net.disconnect_reasons.has("ClientA")), "the client was disconnected")
	var reason: String = net.disconnect_reasons["ClientA"]
	assert_true(reason.contains("property table of scene res://tests/fixtures/net/crate.tscn differs"), reason)
	assert_true(reason.contains("property rotation"), reason)


func test_a_late_joiner_receives_the_current_state() -> void:
	net = NetFixture.new(self, 1)
	net.start_server()
	var managers := net.add_managers()
	var crates: Array[Node2D] = []
	for i in 10:
		var c := _crate("Mover%d" % i)
		managers["Server"].add_child(c)
		crates.append(c)
	var mover := func(_t: int) -> void:
		for i in crates.size():
			crates[i].position += Vector2(1.0 + i, 0.5)
	net.server_session.ticked.connect(mover)
	await _frames(30)
	net.start_clients()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Mover9")))
	net.server_session.ticked.disconnect(mover)
	assert_true(await net.wait_until(func() -> bool:
		for c in crates:
			var r: Node2D = client.get_node("World/" + String(c.name))
			if not _near(r.position, c.position, 0.01):
				return false
		return true), "every moving entity's position matches after the burst")
	assert_gt(crates[0].position.x, 30.0, "they had moved before the client joined")


func test_g4_idle_entities_cost_almost_nothing() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	for i in 100:
		managers["Server"].add_child(_crate("Idle%d" % i))
	assert_true(await net.wait_until(func() -> bool: return net.session(net.clients[0]).entity_count() == 100))
	await _frames(90)
	var before: int = net.server_session.stats()["snapshot_bytes"]
	await _frames(Engine.physics_ticks_per_second + 2)
	var stats: Dictionary = net.server_session.stats()
	var per_s: PackedInt64Array = stats["bytes_sent_per_s"]
	var total := per_s[0] + per_s[1] + per_s[2]
	print("G4 idle: 100 entities, %d B/s total (%d reliable, %d snapshots, %d diagnostics)" % [total, per_s[0], per_s[1], per_s[2]])
	assert_eq(int(stats["snapshot_bytes"]) - before, 0, "no snapshot bytes while nothing changes")
	assert_lt(total, 400, "100 idle entities cost under 400 B/s (pings only)")


func _moving_crates(count: int, scene: PackedScene = CRATE) -> Array[Node2D]:
	var managers := net.add_managers()
	var crates: Array[Node2D] = []
	for i in count:
		var c: Node2D = scene.instantiate()
		c.name = "Move%d" % i
		managers["Server"].add_child(c)
		crates.append(c)
	net.server_session.ticked.connect(func(t: int) -> void:
		for i in crates.size():
			crates[i].position = Vector2(100.0 * i + 37.0 * sin(t * 0.05 + i), 50.0 * cos(t * 0.03 + i))
			crates[i].rotation = t * 0.01 + i)
	return crates


func test_g4_moving_entities_stay_within_the_budget() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	_moving_crates(100)
	assert_true(await net.wait_until(func() -> bool: return net.session(net.clients[0]).entity_count() == 100))
	await _frames(2 * Engine.physics_ticks_per_second + 2)
	var stats: Dictionary = net.server_session.stats()
	var snap: int = (stats["bytes_sent_per_s"] as PackedInt64Array)[1]
	print("G4 moving: 100 entities, %d B/s of snapshots (budget %d)" % [snap, stats["budget_bytes_per_s"]])
	assert_eq(stats["budget_bytes_per_s"], 32768)
	assert_lt(snap, 32768 + 1200, "within the 32 KB/s budget")
	assert_gt(snap, 16000, "and the budget is used")


func test_g4_a_halved_budget_updates_far_entities_less_often() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	net.server_session.budget_bytes_per_s = 16384
	# WP5: interpolated replicas change every render frame, so the updates are counted on a copy of the
	# crate whose transform snaps (the replication under test is the same).
	var probe: Node = CRATE_SNAP.instantiate()
	Net.configure(probe, {"interpolate": false})
	probe.free()
	var crates := _moving_crates(100, CRATE_SNAP)
	# The first ten matter more (priority 10, the stand-in for "near" until WP7's interest scoring).
	for i in 10:
		Net.configure(crates[i], {"priority": 10})
	var client: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool: return net.session(client).entity_count() == 100))
	await _frames(30)
	var updates: Array[int] = []
	updates.resize(100)
	var last: Array[Vector2] = []
	for i in 100:
		last.append((client.get_node("World/Move%d" % i) as Node2D).position)
	for frame in 2 * Engine.physics_ticks_per_second:
		await get_tree().physics_frame
		for i in 100:
			var p := (client.get_node("World/Move%d" % i) as Node2D).position
			if p != last[i]:
				updates[i] += 1
				last[i] = p
	var near := 0.0
	var far := 0.0
	for i in 100:
		if i < 10:
			near += updates[i]
		else:
			far += updates[i]
	near /= 10.0
	far /= 90.0
	var snap: int = (net.server_session.stats()["bytes_sent_per_s"] as PackedInt64Array)[1]
	print("G4 halved budget: near entities %.1f updates, far %.1f in 2 s; %d B/s of snapshots (budget 16384)" % [near, far, snap])
	assert_gt(near, 3.0 * far, "priority 10 entities update far more often")
	assert_gt(far, 1.0, "the far ones still update")
	assert_lt(snap, 16384 + 1200, "within the halved budget")


func test_fields_converge_under_loss() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	var client: Node = net.clients[0]
	var emu := NetEmulation.new()
	emu.send_loss = 0.25
	emu.receive_loss = 0.25
	emu.send_latency_ms = 30.0
	emu.receive_latency_ms = 30.0
	emu.seed = 11
	net.set_emulation(client, emu)
	var crates := _moving_crates(5)
	assert_true(await net.wait_until(func() -> bool: return net.session(client).entity_count() == 5))
	await _frames(60)
	for c in crates:
		c.set_meta("stop", true)
	# Stop moving: the latest values must arrive although snapshots were lost.
	for conn in net.server_session.ticked.get_connections():
		net.server_session.ticked.disconnect(conn["callable"])
	crates[2].get_node("Sprite").flip_v = true
	assert_true(await net.wait_until(func() -> bool:
		for c in crates:
			var r: Node2D = client.get_node("World/" + String(c.name))
			if not _near(r.position, c.position, 0.001) or absf(angle_difference(r.rotation, c.rotation)) > 0.001:
				return false
		return (client.get_node("World/Move2/Sprite") as Sprite2D).flip_v, 4.0), "every field converged")
	var stats: Dictionary = net.session(client).stats()
	print("loss: %d snapshots received, %d stale" % [stats["snapshots_received"], stats["snapshots_stale"]])


func test_polling_benchmark_1000_entities_8_hot_properties() -> void:
	net = NetFixture.new(self, 0)
	net.start_server()
	var managers := net.add_managers()
	var nodes: Array[Node2D] = []
	for i in 1000:
		var b: Node2D = BENCH.instantiate()
		b.name = "B%d" % i
		managers["Server"].add_child(b)
		nodes.append(b)
	assert_true(await net.wait_until(func() -> bool: return net.server_session.entity_count() == 1000, 8.0))
	var samples: Array[float] = []
	var sampler := func(_t: int) -> void:
		samples.append(float(net.server_session.stats()["poll_ms"]))
	net.server_session.ticked.connect(sampler)
	for frame in 61:
		for b in nodes:
			b.position.x += 1.0
		await get_tree().physics_frame
	net.server_session.ticked.disconnect(sampler)
	samples.pop_front()
	var total := 0.0
	for s in samples:
		total += s
	var avg := total / samples.size()
	print("BENCHMARK: polling 1000 entities x 8 hot properties: %.3f ms per tick (average of %d ticks; stats poll_ms_avg %.3f)" % [avg, samples.size(), net.server_session.stats()["poll_ms_avg"]])
	assert_lt(avg, 100.0, "a loose bound; the number above is the measurement")


func test_g0_a_manager_without_a_session_sends_and_polls_nothing() -> void:
	var before := NetSession.packets_sent_total()
	var world := NetManager.new()
	add_child(world)
	world.add_child(LEVEL.instantiate())
	await _frames(20)
	assert_eq(NetSession.packets_sent_total(), before, "nothing was sent")
	assert_false(Net.is_online())
	assert_eq(world.entities, 0, "nothing spawned, nothing polled")
