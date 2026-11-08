extends TestCase
## WP14 (docs/PLAN_NETCODE.md 2.11, gates G2, G4 and G8): the Ballpit demo (`demos/ballpit/`) under
## NetFixture: hundreds of server-owned RigidBody2D balls settle and cost nothing asleep, a predicted
## paddle pushes them, a halved budget degrades the far balls and keeps the near ones, and the README's
## listings are the files. Every branch holds its own copy of the pit; the branches share one physics
## space, so each client's pit moves its bodies to collision layers of their own (`layer_shift`).

const PIT := preload("res://demos/ballpit/pit.tscn")
const MANY := 300
const FEW := 120

var net: NetFixture


func after_each() -> void:
	Input.action_release("down")
	Input.action_release("up")
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## A pit in every branch (the clients' on shifted collision layers); every pit starts.
func _setup(f: NetFixture, balls: int) -> void:
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for i in branches.size():
		var pit: BallpitSpawner = PIT.instantiate()
		pit.name = "Pit"
		pit.ball_count = balls
		pit.dedicated = f.host_mode == NetFixture.HostMode.DEDICATED
		pit.layer_shift = 3 * i
		branches[i].add_child(pit)
	for branch in branches:
		(branch.get_node("Pit") as BallpitSpawner).start()


func _balls(branch: Node) -> Array[RigidBody2D]:
	var out: Array[RigidBody2D] = []
	for n in branch.get_node("Pit/NetManager").get_children():
		if n is RigidBody2D and not n.is_queued_for_deletion():
			out.append(n)
	return out


func _all_asleep(branch: Node) -> bool:
	for b in _balls(branch):
		if not b.sleeping:
			return false
	return true


## Awaits a pile that stays asleep for half a second (a ball rolling slowly on the floor dozes off and
## wakes again for a while before it stops). False on timeout.
func _settle(f: NetFixture, timeout: float) -> bool:
	var still := [0]
	return await f.wait_until(func() -> bool:
		still[0] = still[0] + 1 if _all_asleep(f.server) else 0
		return still[0] >= Engine.physics_ticks_per_second / 2, timeout)


## The largest distance between a server ball and its client copy (rotation counted as 10 px/rad).
func _max_error(server: Node, client: Node) -> float:
	var worst := 0.0
	for b in _balls(server):
		var c := client.get_node_or_null("Pit/NetManager/" + String(b.name)) as RigidBody2D
		if c == null:
			return INF
		worst = maxf(worst, b.position.distance_to(c.position))
		worst = maxf(worst, 10.0 * absf(angle_difference(b.rotation, c.rotation)))
	return worst


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 3
	return e


func test_balls_settle_converge_and_cost_nothing_asleep() -> void:
	timeout_sec = 40.0
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var c: Node = f.clients[0]
		_setup(f, MANY)
		var expected := MANY + f.players().size()
		assert_true(await f.wait_until(func() -> bool: return f.session(c).entity_count() == expected, 5.0), "every ball and paddle reached the client")
		var t0 := Time.get_ticks_msec()
		assert_true(await _settle(f, 8.0), "the pile settles: every ball sleeps")
		var settled_s := (Time.get_ticks_msec() - t0) / 1000.0
		assert_true(await f.wait_until(func() -> bool: return _max_error(f.server, c) < 0.05, 3.0), "the client converged: %.4f" % _max_error(f.server, c))
		for b in _balls(c):
			assert_true(b.freeze, "replica %s is frozen" % b.name)
			assert_false(b.is_physics_processing(), "replica %s does not simulate" % b.name)
		# The last moves' hashes go out (two bodies sampled a tick: 300 take 2.5 s), then a second of a
		# sleeping pile.
		await _frames(3 * Engine.physics_ticks_per_second)
		var before: Dictionary = f.server_session.stats()
		await _frames(Engine.physics_ticks_per_second + 2)
		var after: Dictionary = f.server_session.stats()
		var per_s: PackedInt64Array = after["bytes_sent_per_s"]
		var total := per_s[0] + per_s[1] + per_s[2]
		print("ballpit settle (mode %d): %d balls asleep after %.1f s; asleep: %d B/s to the client (%d reliable, %d snapshots, %d hashes and pings)" % [f.host_mode, MANY, settled_s, total, per_s[0], per_s[1], per_s[2]])
		assert_eq(int(after["changes"]) - int(before["changes"]), 0, "no ball recorded a change: zero dirty fields")
		assert_eq(int(after["snapshot_bytes"]) - int(before["snapshot_bytes"]), 0, "no snapshot bytes for a sleeping pile")
		assert_lt(total, 400, "a sleeping pile costs under 400 B/s (pings only)")
		# The trust toggle: the paddles come back as client_owned entities, amber in the overlay.
		var pit: BallpitSpawner = f.server.get_node("Pit")
		pit.set_trusted_paddles(true)
		var cid := f.peer_id(c)
		assert_true(await f.wait_until(func() -> bool: return c.has_node("Pit/NetManager/trusted_paddle%d" % cid) and not c.has_node("Pit/NetManager/paddle%d" % cid)), "the paddles were spawned again as trusted ones")
		var roles := {}
		for row: Dictionary in f.session(c).overlay_rows():
			roles[row["role"]] = true
		assert_true(roles.has("client_owned"), "the overlay shows a client_owned paddle: %s" % roles)
		pit.set_trusted_paddles(false)
	)
	assert_true(ok, "fixtures connected")


func test_a_paddle_push_is_predicted_and_the_balls_follow() -> void:
	timeout_sec = 35.0
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	_setup(net, FEW)
	var cid := net.peer_id(c)
	assert_true(await net.wait_until(func() -> bool: return net.session(c).entity_count() == FEW + 1, 5.0))
	assert_true(await _settle(net, 8.0), "settled")
	assert_true(await net.wait_until(func() -> bool: return _max_error(net.server, c) < 0.05, 3.0), "converged")
	await _frames(Engine.physics_ticks_per_second)
	var sp: Node2D = net.server.get_node("Pit/NetManager/paddle%d" % cid)
	var cp: Node2D = c.get_node("Pit/NetManager/paddle%d" % cid)
	var start := cp.position
	var server_start := {}
	var client_start := {}
	for b in _balls(net.server):
		server_start[b.name] = b.position
		client_start[b.name] = (c.get_node("Pit/NetManager/" + String(b.name)) as Node2D).position
	Input.action_press("down")
	await get_tree().physics_frame
	assert_gt(cp.position.y, start.y, "the owner's paddle moved on the tick after the press (prediction)")
	assert_eq(sp.position.y, start.y, "the server has not received the input yet")
	var server_moved := -1
	var client_moved := -1
	var interacting := PackedInt64Array()
	var near_ok := true
	var far_left_out := 0
	for t in 90:
		await get_tree().physics_frame
		if t == 60:
			Input.action_release("down")
			Input.action_press("up")
		for b in _balls(net.server):
			if server_moved < 0 and b.position.distance_to(server_start[b.name]) > 0.5:
				server_moved = t
			var cb := c.get_node("Pit/NetManager/" + String(b.name)) as Node2D
			if client_moved < 0 and cb.position.distance_to(client_start[b.name]) > 0.5:
				client_moved = t
		if t >= 40:
			# The paddle is in the pile: the server marks the balls near it as interacting.
			var marked_now := net.server_session.interacting(cid)
			if marked_now.size() > interacting.size():
				interacting = marked_now
			var radius: float = (net.server.get_node("Pit/NetManager") as NetManager).interaction_radius
			for b in _balls(net.server):
				var d := b.global_position.distance_to(sp.global_position)
				var marked := marked_now.has(Net.net_id(b))
				if marked and d > radius + 20.0:
					near_ok = false
				if not marked and d > radius + 20.0:
					far_left_out += 1
	assert_gt(server_moved, -1, "the paddle pushed balls on the server")
	assert_gt(client_moved, -1, "the client saw them move")
	var seen := client_moved - server_moved
	assert_gt(interacting.size(), 0, "balls near the paddle are interacting")
	assert_true(near_ok, "every interacting ball is within the radius")
	assert_gt(far_left_out, 0, "far balls are not interacting")
	# The paddle went back up out of the pile (a resting paddle inside it would keep nudging the balls).
	Input.action_release("up")
	var t1 := Time.get_ticks_msec()
	assert_true(await _settle(net, 12.0), "the pushed balls settle on the server")
	var settle_ticks := roundi((Time.get_ticks_msec() - t1) / 1000.0 * Engine.physics_ticks_per_second)
	var ticks := 0
	var coarse := -1
	while _max_error(net.server, c) >= 0.05 and ticks < 180:
		if coarse < 0 and _max_error(net.server, c) < 0.5:
			coarse = ticks
		await get_tree().physics_frame
		ticks += 1
	if coarse < 0:
		coarse = ticks
	assert_lt(ticks, 180, "the pushed balls converge on the client: %.4f" % _max_error(net.server, c))
	print("ballpit push: the owner's paddle moved at once; %d interacting balls at the deepest point; the owner saw balls move %d ticks after its press and %d ticks after the server moved them (50 ms each way); the server settled (asleep for half a second) %d ticks after the push ended; the client was within 0.5 px %d ticks and within 0.05 px %d ticks after that" % [interacting.size(), client_moved + 1, seen, settle_ticks, coarse, ticks])
	assert_lt(seen, 20, "a push is seen within one round trip plus the jitter buffer")
	for b in _balls(c):
		assert_true(b.freeze and not b.is_physics_processing(), "no double simulation of %s" % b.name)
	var stats: Dictionary = net.session(c).stats()
	assert_eq(int(stats["rollbacks"]), 0, "the paddle's prediction never fought the balls")


func test_a_halved_budget_degrades_far_balls_and_keeps_near_ones() -> void:
	timeout_sec = 20.0
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	_setup(net, MANY)
	var c: Node = net.clients[0]
	var cid := net.peer_id(c)
	var budget := 16384
	net.server_session.budget_bytes_per_s = budget
	assert_true(await net.wait_until(func() -> bool: return net.session(c).entity_count() == MANY + 1, 5.0))
	var pit: BallpitSpawner = net.server.get_node("Pit")
	var sp: Node2D = net.server.get_node("Pit/NetManager/paddle%d" % cid)
	sp.position = Vector2(320, 330)
	var balls := _balls(net.server)
	var ids := {}
	for b in balls:
		ids[Net.net_id(b)] = b
	var near_sent := 0
	var near_slots := 0
	var far_sent := 0
	var far_slots := 0
	var last := {}
	# Each ball's pose at the last two reads: the snapshot pass of tick t-1 (whose sends and
	# interacting list are read at the start of frame t) recorded the pose read at frame t-1, so the
	# ball had something to send then when that pose differs from the one read at frame t-2.
	var pose := {}
	var before: Dictionary = {}
	var radius: float = (net.server.get_node("Pit/NetManager") as NetManager).interaction_radius
	var near_ok := true
	for t in 150:
		if t % 20 == 0:
			pit.shake()
		await get_tree().physics_frame
		if t == 30:
			before = net.server_session.stats()
		var marked := net.server_session.interacting(cid)
		for id: int in ids:
			var b: RigidBody2D = ids[id]
			var n: int = net.server_session.entity_sends(b, cid)
			var now := Vector3(b.position.x, b.position.y, b.rotation)
			var hist: Array = pose.get(id, [now, now])
			var changed: bool = not (hist[0] as Vector3).is_equal_approx(hist[1])
			if t >= 30 and last.has(id) and changed:
				var sent := 1 if n > last[id] else 0
				if marked.has(id):
					near_slots += 1
					near_sent += sent
					# The pass ran before this tick's physics step moved the balls: allow one step.
					if b.global_position.distance_to(sp.global_position) > radius + 20.0:
						near_ok = false
				else:
					far_slots += 1
					far_sent += sent
			last[id] = n
			pose[id] = [now, hist[0]]
	var after: Dictionary = net.server_session.stats()
	var per_s: PackedInt64Array = after["bytes_sent_per_s"]
	var snap := per_s[1]
	var near_rate := float(near_sent) / maxf(1.0, near_slots)
	var far_rate := float(far_sent) / maxf(1.0, far_slots)
	var sent_entities := int(after["snapshot_entities_sent"]) - int(before["snapshot_entities_sent"])
	var sent_bytes := int(after["snapshot_bytes"]) - int(before["snapshot_bytes"])
	var snaps := int(after["snapshots_sent"]) - int(before["snapshots_sent"])
	print("ballpit halved budget: %d B/s of snapshots (budget %d), deferred %d ent/s %d fields/s; near balls sent on %.0f%% of ticks (%d slots), far moving balls %.0f%% (%d slots); %.1f bytes per body update, %.0f bytes per snapshot; hashes and pings %d B/s (%.1f%% of the budget); interaction pass %.3f ms" % [snap, budget, after["deferred_entities_per_s"], after["deferred_fields_per_s"], 100.0 * near_rate, near_slots, 100.0 * far_rate, far_slots, float(sent_bytes) / maxf(1.0, sent_entities), float(sent_bytes) / maxf(1.0, snaps), per_s[2], 100.0 * per_s[2] / budget, after["interaction_ms"]])
	assert_lt(snap, budget + 1200, "within the halved budget plus one MTU")
	assert_gt(int(after["deferred_entities_per_s"]), 0, "the degradation counter rose")
	assert_gt(near_slots, 0, "some balls were near the paddle")
	assert_true(near_ok, "interacting balls are within the radius")
	assert_gt(near_rate, 0.8, "interacting balls update (nearly) every tick")
	assert_lt(far_rate, near_rate / 4.0, "far balls are degraded")
	assert_lt(per_s[2], budget / 10, "hash telemetry and pings stay under a tenth of the budget")


func test_g8_the_readme_listings_are_the_files() -> void:
	var readme := FileAccess.get_file_as_string("res://demos/ballpit/README.md")
	assert_false(readme.is_empty(), "the README exists")
	var listed := {}
	var lines := readme.split("\n")
	var i := 0
	while i < lines.size():
		var line := lines[i]
		if line.begins_with("```") and line.contains(" file="):
			var path := line.get_slice("file=", 1).strip_edges()
			var body := PackedStringArray()
			i += 1
			while i < lines.size() and not lines[i].begins_with("```"):
				body.append(lines[i])
				i += 1
			var on_disk := FileAccess.get_file_as_string("res://" + path).strip_edges(false, true)
			assert_eq("\n".join(body), on_disk, "the README's listing of %s" % path)
			listed[path] = true
		i += 1
	for f in DirAccess.get_files_at("res://demos/ballpit"):
		if f.ends_with(".gd") or f.ends_with(".tscn"):
			assert_true(listed.has("demos/ballpit/" + f), "the README lists %s" % f)
