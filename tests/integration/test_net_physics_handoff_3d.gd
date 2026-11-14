extends TestCase
## WP19 slice C in 3D (docs/PLAN_NETCODE.md 12.4, gate G10 for `handoff`): the 3D twins of
## `test_net_physics_handoff.gd`'s push, dispute and holder-leaves scenarios: a client's predicted player
## walks into a box and moves it at once, the box converges everywhere and returns to the server at
## rest; two claims in one tick agree on one winner; a holder that leaves gives the box back at once.
## Lengths are metres. Every body entering a branch's manager goes to that branch's collision layer.

const BOX := preload("res://tests/fixtures/net3d/box_body_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const PUSHER := preload("res://tests/fixtures/handoff/pusher_3d.tscn")
const MODES := preload("res://tests/integration/test_net_physics_modes.gd")
## A replica within half a `mm` step on each axis of the value sent.
const SETTLED := 0.001
## The loser's copy blends (see the 2D file's BLEND_STEP): 15 % of what two pushers at 1.5 m/s move
## apart in a 100 ms round trip, 4.5 cm, in a frame beyond the box's own motion.
const BLEND_STEP := 0.15 * 2.0 * 1.5 * 0.1

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


func _managers(f: NetFixture) -> Dictionary:
	var managers := f.add_managers("World", func(m: NetManager) -> void: m.physics = NetEntity.PHYSICS_HANDOFF)
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: MODES.layer(n, bit))
	return managers


func _fixture(mode: NetFixture.HostMode) -> NetFixture:
	var f := NetFixture.new(self, 2, mode)
	for c in f.clients:
		f.set_emulation(c, _link(50.0))
	f.start()
	assert_true(await f.wait_connected())
	return f


func _box(world: Node, node_name: String, at: Vector3) -> RigidBody3D:
	var b: RigidBody3D = BOX.instantiate()
	b.name = node_name
	b.position = at
	world.add_child(b)
	return b


func _pusher(world: Node, peer: int, node_name: String, at: Vector3, direction: float) -> CharacterBody3D:
	var p: CharacterBody3D = Net.spawn_for(peer, PUSHER, world, node_name)
	p.position = at
	p.direction = direction
	return p


func _copies(f: NetFixture, path: String) -> Array:
	var out: Array = []
	for c: Node in f.clients:
		assert_true(await f.wait_until(func() -> bool: return c.has_node(path)), "%s reached %s" % [path, c.name])
		out.append(c.get_node(path))
	return out


## (simulated_by, handoff_seq) of the body at `path` on every peer, server first.
func _views(f: NetFixture, path: String) -> Array:
	var out: Array = []
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for b in branches:
		var node := b.get_node(path)
		var row := {}
		for r: Dictionary in f.session(b).overlay_rows():
			if r["node"] == node:
				row = r
		out.append([int(row.get("simulated_by", -9)), int(row.get("handoff_seq", -9))])
	return out


func _agree(f: NetFixture, path: String, peer: int, seq: int) -> bool:
	return _views(f, path).all(func(v: Array) -> bool: return v[0] == peer and v[1] == seq)


func test_a_pushed_box_moves_at_once_converges_and_returns_at_rest_in_3d() -> void:
	timeout_sec = 30.0
	add_child(FLOOR.instantiate())
	var results := {}
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = await _fixture(mode)
		var managers := _managers(net)
		var world: Node = managers["Server"]
		var a: Node = net.clients[0]
		var aid := net.peer_id(a)
		var sb := _box(world, "Box", Vector3(1.0, 0.25, 0.0))
		_pusher(world, aid, "P", Vector3(0.0, 0.0, 0.0), 1.0)
		var boxes := await _copies(net, "World/Box")
		var pa: CharacterBody3D = (await _copies(net, "World/P"))[0]
		var ba: RigidBody3D = boxes[0]
		var bb: RigidBody3D = boxes[1]
		assert_true(await net.wait_until(func() -> bool: return pa.is_on_floor() and sb.sleeping, 5.0), "everything rests")
		await _frames(10)
		assert_true(_agree(net, "World/Box", 1, 0), "the server holds it: %s" % [_views(net, "World/Box")])
		var r0: int = net.session(a).stats()["rollbacks"]
		var late0: int = net.server_session.stats()["handoff_late_states"]
		var x0 := sb.position.x
		Input.action_press("right")
		assert_true(await net.wait_until(func() -> bool: return pa.first_contact_frame >= 0, 3.0), "the pusher reached the box")
		var contact: int = pa.first_contact_frame
		var moved := {"pusher": -1, "server": -1, "other": -1}
		var copies := {"pusher": ba, "server": sb, "other": bb}
		for t in 60:
			for k: String in copies:
				if moved[k] < 0 and (copies[k] as Node3D).position.x > x0 + 0.0005:
					moved[k] = Engine.get_physics_frames() - contact
			if t == 30:
				assert_true(_agree(net, "World/Box", aid, 1), "A holds it at sequence 1 everywhere: %s" % [_views(net, "World/Box")])
				assert_true(not ba.freeze and sb.freeze and bb.freeze, "live on A only")
			await get_tree().physics_frame
		Input.action_release("right")
		var pushed := sb.position.x - x0
		assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/Box", 1, 2), 5.0), "back with the server at rest: %s" % [_views(net, "World/Box")])
		assert_true(ba.freeze and bb.freeze and not sb.freeze, "the server simulates it again")
		assert_true(await net.wait_until(func() -> bool: return sb.sleeping and ba.position.distance_to(sb.position) < SETTLED and bb.position.distance_to(sb.position) < SETTLED, 5.0), "every copy converges (%s, A %s, B %s)" % [sb.position, ba.position, bb.position])
		var bytes0: int = net.server_session.stats()["snapshot_bytes"]
		await _frames(30)
		assert_eq(int(net.server_session.stats()["snapshot_bytes"]) - bytes0, 0, "asleep, no snapshot bytes")
		results[mode] = {"moved": moved, "pushed": pushed, "rollbacks": int(net.session(a).stats()["rollbacks"]) - r0, "late": int(net.server_session.stats()["handoff_late_states"]) - late0}
		net.teardown()
		net = null
	for mode: int in results:
		var r: Dictionary = results[mode]
		var m: Dictionary = r["moved"]
		print("handoff push in 3D (mode %d, 50 ms each way): the box moved %d frame(s) after the contact on the pusher, %d on the server, %d on the other client; pushed %.3f m; %d rollbacks; %d holder states came late at the server" % [mode, m["pusher"], m["server"], m["other"], r["pushed"], r["rollbacks"], r["late"]])
		assert_true(m["pusher"] >= 0 and m["pusher"] <= 2, "on the pusher within two ticks of the contact (%d)" % m["pusher"])
		assert_gt(m["server"], m["pusher"], "later on the server")
		assert_gt(r["pushed"], 0.3, "the push moved the box")
		assert_lt(r["rollbacks"], 10, "no rollback storm for the pushing player: fewer than one in 15 ticks")


func test_two_claims_in_one_tick_agree_on_the_winner_in_3d() -> void:
	timeout_sec = 30.0
	add_child(FLOOR.instantiate())
	var results := {}
	for host in [false, true]:
		net = await _fixture(NetFixture.HostMode.HOST_PLAYER if host else NetFixture.HostMode.DEDICATED)
		var managers := _managers(net)
		var world: Node = managers["Server"]
		var a: Node = net.clients[0]
		var b: Node = net.clients[1]
		var sb := _box(world, "Box", Vector3(1.0, 0.25, 0.0))
		_pusher(world, 1 if host else net.peer_id(a), "L", Vector3(0.0, 0.0, 0.0), 1.0)
		_pusher(world, net.peer_id(b), "R", Vector3(2.0, 0.0, 0.0), -1.0)
		var boxes := await _copies(net, "World/Box")
		var pr: CharacterBody3D = (await _copies(net, "World/R"))[1]
		assert_true(await net.wait_until(func() -> bool: return pr.is_on_floor() and sb.sleeping, 5.0), "everything rests")
		await _frames(10)
		var copies: Array = [sb, boxes[0], boxes[1]]
		Input.action_press("right")
		var xs: Array = [[], [], []]
		for t in 60:
			await get_tree().physics_frame
			for k in 3:
				xs[k].append((copies[k] as Node3D).position.x)
		Input.action_release("right")
		var settled := await net.wait_until(func() -> bool:
			var v := _views(net, "World/Box")
			return v.all(func(x: Array) -> bool: return x == v[0]), 3.0)
		var views := _views(net, "World/Box")
		assert_true(settled, "every peer agrees on the holder and the number: %s" % [views])
		var winner: int = views[0][0]
		var loser := 2 if winner != net.peer_id(b) else 1
		var ref := 0 if winner == 1 else (1 if winner == net.peer_id(a) else 2)
		var largest := 0.0
		for i in range(1, xs[0].size()):
			largest = maxf(largest, absf(xs[loser][i] - xs[loser][i - 1]) - absf(xs[ref][i] - xs[ref][i - 1]))
		results[host] = {"winner": winner, "seq": views[0][1], "a": net.peer_id(a), "largest": largest, "frozen": (copies[loser] as RigidBody3D).freeze}
		net.teardown()
		net = null
	for host: bool in results:
		var r: Dictionary = results[host]
		print("handoff dispute in 3D (%s against client B): winner peer %d, sequence %d; the loser's copy stepped at most %.4f m beyond the winner's motion" % ["the host's player" if host else "client A", r["winner"], r["seq"], r["largest"]])
		assert_true(r["frozen"], "the loser shows the box frozen")
		assert_lt(r["largest"], BLEND_STEP, "the loser blends")
	assert_eq(results[false]["winner"], results[false]["a"], "the lower peer id wins a tie")
	assert_eq(results[false]["seq"], 1)
	assert_eq(results[true]["winner"], 1, "the host's player touching it keeps it")
	assert_eq(results[true]["seq"], 0)


func test_a_holder_that_leaves_gives_the_box_back_at_once_in_3d() -> void:
	timeout_sec = 15.0
	add_child(FLOOR.instantiate())
	net = await _fixture(NetFixture.HostMode.DEDICATED)
	var managers := _managers(net)
	var world: Node = managers["Server"]
	var sb := _box(world, "Box", Vector3(0.0, 2.0, 0.0))
	sb.gravity_scale = 0.0
	sb.linear_damp_mode = RigidBody3D.DAMP_MODE_REPLACE
	var ba: RigidBody3D = (await _copies(net, "World/Box"))[0]
	await _frames(5)
	assert_true(Net.claim(ba))
	ba.gravity_scale = 0.0
	ba.linear_damp_mode = RigidBody3D.DAMP_MODE_REPLACE
	ba.linear_velocity = Vector3(1.5, 0.0, 0.0)
	assert_true(await net.wait_until(func() -> bool: return sb.position.x > 0.2, 3.0), "the server's copy follows the holder")
	net.drop_client(0)
	var xs: Array = [sb.position.x]
	var frames := -1
	for t in 20:
		await get_tree().physics_frame
		xs.append(sb.position.x)
		if frames < 0 and not sb.freeze:
			frames = t + 1
	var jump := 0.0
	for i in range(1, xs.size()):
		jump = maxf(jump, absf(xs[i] - xs[i - 1]) - 1.5 / 60.0)
	print("handoff in 3D, the holder leaves: the server simulates the box %d frame(s) later at %.2f m/s, stepping at most %.4f m beyond 1.5 m/s" % [frames, sb.linear_velocity.x, jump])
	assert_true(frames >= 1 and frames <= 3, "at once (%d)" % frames)
	assert_almost_eq(sb.linear_velocity.x, 1.5, 0.02, "still moving")
	assert_lt(jump, 0.005, "without a jump")
