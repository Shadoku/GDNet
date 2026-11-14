extends TestCase
## WP19 slice C (docs/PLAN_NETCODE.md 12.4): the `handoff` physics mode in 2D. A body is simulated by
## whoever interacts with it: a client whose predicted player walks into a crate claims it, moves it at
## once and holds it until it rests, then the server has it again (and a resting pile nobody touches is
## a `server` pile). Two claims in one tick agree on one winner by ownership sequence number; late
## states of a previous holder are dropped; a holder that leaves gives the body back at once; game code
## claims a carried item with `Net.claim`; a held body claims what it pushes; another client's dead
## reckoning follows the holder.
##
## The branches share one physics space: every body entering a branch's manager goes to that branch's
## collision layer (bit 1 + branch index; the floor is layer 1), so the copies never touch.

const CRATE := preload("res://tests/fixtures/physics/crate.tscn")
const FLOOR := preload("res://tests/fixtures/physics/floor.tscn")
const PUSHER := preload("res://tests/fixtures/handoff/pusher.tscn")
const PUCK := preload("res://tests/fixtures/handoff/puck.tscn")
const MODES := preload("res://tests/integration/test_net_physics_modes.gd")
## A replica lands within half a 0.1 px step of the value sent (D17).
const SETTLED := 0.1

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


## Handoff by the manager's default; per-branch collision layers.
func _managers(f: NetFixture) -> Dictionary:
	var managers := f.add_managers("World", func(m: NetManager) -> void: m.physics = NetEntity.PHYSICS_HANDOFF)
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void: MODES.layer(n, bit))
	return managers


func _start(clients: int, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED) -> Dictionary:
	add_child(FLOOR.instantiate())
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	return _managers(net)


func _add(world: Node, scene: PackedScene, node_name: String, at: Vector2) -> Node2D:
	var n: Node2D = scene.instantiate()
	n.name = node_name
	n.position = at
	world.add_child(n)
	return n


## A pusher for `peer` at `at`, walking the way of `direction` while "right" is held.
func _spawn_pusher(_f: NetFixture, world: Node, peer: int, node_name: String, at: Vector2, direction: float) -> CharacterBody2D:
	var p: CharacterBody2D = Net.spawn_for(peer, PUSHER, world, node_name)
	p.position = at
	p.direction = direction
	return p


func _copies(f: NetFixture, path: String) -> Array:
	var out: Array = []
	for c: Node in f.clients:
		assert_true(await f.wait_until(func() -> bool: return c.has_node(path)), "%s reached %s" % [path, c.name])
		out.append(c.get_node(path))
	return out


## The overlay row of `node` in `branch`'s session.
func _row(f: NetFixture, branch: Node, node: Node) -> Dictionary:
	for r: Dictionary in f.session(branch).overlay_rows():
		if r["node"] == node:
			return r
	return {}


## (simulated_by, handoff_seq) of the body at `path` on every peer, server first.
func _views(f: NetFixture, path: String) -> Array:
	var out: Array = []
	var branches: Array[Node] = [f.server]
	branches.append_array(f.clients)
	for b in branches:
		var r := _row(f, b, b.get_node(path))
		out.append([int(r.get("simulated_by", -9)), int(r.get("handoff_seq", -9))])
	return out


## Every peer shows `path` simulated by `peer` at sequence number `seq`.
func _agree(f: NetFixture, path: String, peer: int, seq: int) -> bool:
	return _views(f, path).all(func(v: Array) -> bool: return v[0] == peer and v[1] == seq)


# --- a push --------------------------------------------------------------------------------------------


func _push_case(f: NetFixture) -> Dictionary:
	var managers := _managers(f)
	var world: Node = managers["Server"]
	var a: Node = f.clients[0]
	var aid := f.peer_id(a)
	var sc: RigidBody2D = _add(world, CRATE, "Crate", Vector2(60, 290))
	var sp := _spawn_pusher(f, world, aid, "P", Vector2(0, 288), 1.0)
	var crates := await _copies(f, "World/Crate")
	var pushers := await _copies(f, "World/P")
	var ca: RigidBody2D = crates[0]
	var cb: RigidBody2D = crates[1]
	var pa: CharacterBody2D = pushers[0]
	assert_true(await f.wait_until(func() -> bool: return pa.is_on_floor() and sc.sleeping, 5.0), "everything rests")
	await _frames(10)
	assert_true(ca.freeze and cb.freeze and not sc.freeze, "the server holds an untouched crate")
	assert_true(_agree(f, "World/Crate", 1, 0), "simulated by the server everywhere, sequence 0: %s" % [_views(f, "World/Crate")])
	var r0: int = f.session(a).stats()["rollbacks"]
	var late0: int = f.server_session.stats()["handoff_late_states"]
	var x0 := sc.position.x
	Input.action_press("right")
	assert_true(await f.wait_until(func() -> bool: return pa.first_contact_frame >= 0, 3.0), "the pusher reached the crate")
	var contact: int = pa.first_contact_frame
	var moved := {"pusher": -1, "server": -1, "other": -1}
	var copies := {"pusher": ca, "server": sc, "other": cb}
	for t in 90:
		for k: String in copies:
			if moved[k] < 0 and (copies[k] as Node2D).position.x > x0 + 0.05:
				moved[k] = Engine.get_physics_frames() - contact
		if t == 30:
			assert_true(_agree(f, "World/Crate", aid, 1), "A holds it at sequence 1 on every peer: %s" % [_views(f, "World/Crate")])
			assert_false(ca.freeze, "A's copy is live")
			assert_true(sc.freeze and cb.freeze, "the server's and B's copies are frozen")
		await get_tree().physics_frame
	Input.action_release("right")
	var pushed := sc.position.x - x0
	var t_release := Time.get_ticks_msec()
	assert_true(await f.wait_until(func() -> bool: return _agree(f, "World/Crate", 1, 2), 5.0), "it comes back to the server at rest, sequence 2: %s" % [_views(f, "World/Crate")])
	var back_ms := Time.get_ticks_msec() - t_release
	assert_true(ca.freeze and cb.freeze and not sc.freeze, "the server simulates it again; every client shows it frozen")
	assert_true(await f.wait_until(func() -> bool: return sc.sleeping and absf(ca.position.x - sc.position.x) < SETTLED and absf(cb.position.x - sc.position.x) < SETTLED, 5.0), "every copy converges on the server's (%s, A %s, B %s)" % [sc.position, ca.position, cb.position])
	var bytes0: int = f.server_session.stats()["snapshot_bytes"]
	await _frames(30)
	var rolled: int = int(f.session(a).stats()["rollbacks"]) - r0
	assert_eq(int(f.server_session.stats()["snapshot_bytes"]) - bytes0, 0, "asleep, it costs no snapshot bytes")
	var st: Dictionary = f.session(a).stats()
	assert_eq(int(st["handoff_held"]), 0, "A holds nothing now")
	assert_gt(int(st["handoff_claims"]), 0, "A claimed")
	assert_gt(int(f.server_session.stats()["handoff_returns"]), 0, "the server counted the return")
	sp.queue_free()
	sc.queue_free()
	await _frames(5)
	var late: int = int(f.server_session.stats()["handoff_late_states"]) - late0
	return {"moved": moved, "pushed": pushed, "back_ms": back_ms, "rollbacks": rolled, "late": late}


func test_a_pushed_crate_moves_at_once_converges_and_returns_to_the_server_at_rest() -> void:
	timeout_sec = 40.0
	var results := {}
	add_child(FLOOR.instantiate())
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		for c in f.clients:
			f.set_emulation(c, _link(50.0))
		results[f.host_mode] = await _push_case(f))
	assert_true(ok)
	for mode: int in results:
		var r: Dictionary = results[mode]
		var m: Dictionary = r["moved"]
		print("handoff push (mode %d, 50 ms each way): the crate moved %d frame(s) after the contact on the pusher, %d on the server, %d on the other client; pushed %.1f px; back with the server %d ms after the key was let go; %d rollbacks; %d holder states came late at the server" % [mode, m["pusher"], m["server"], m["other"], r["pushed"], r["back_ms"], r["rollbacks"], r["late"]])
		assert_between(m["pusher"], 0, 2, "on the pusher within two ticks of the contact")
		assert_gt(m["server"], m["pusher"], "later on the server")
		assert_gt(r["pushed"], 30.0, "the push moved the crate")
		assert_lt(r["rollbacks"], 10, "no rollback storm for the pushing player: fewer than one in 15 ticks of pushing and coming to rest (48 before the holder stamped and quantised its states)")


func assert_between(v: int, lo: int, hi: int, msg: String) -> void:
	assert_true(v >= lo and v <= hi, "%s (%d not in %d..%d)" % [msg, v, lo, hi])


# --- an untouched pile -----------------------------------------------------------------------------------


func test_an_untouched_handoff_pile_is_a_server_pile() -> void:
	timeout_sec = 15.0
	var managers := await _start(1)
	var world: Node = managers["Server"]
	var crates: Array = []
	for i in 20:
		@warning_ignore("integer_division")
		crates.append(_add(world, CRATE, "C%d" % i, Vector2(-80 + (i % 10) * 22, 290 - (i / 10) * 20)))
	var c: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool: return net.session(c).entity_count() == 20 and crates.all(func(b: RigidBody2D) -> bool: return b.sleeping), 6.0), "the pile settles and reaches the client")
	await _frames(30)
	var bytes0: int = net.server_session.stats()["snapshot_bytes"]
	await _frames(60)
	assert_eq(int(net.server_session.stats()["snapshot_bytes"]) - bytes0, 0, "asleep: no snapshot bytes")
	for b: RigidBody2D in crates:
		assert_false(b.freeze, "the server simulates %s" % b.name)
		assert_true((c.get_node("World/" + String(b.name)) as RigidBody2D).freeze, "the client shows %s frozen" % b.name)
		assert_true(_agree(net, "World/" + String(b.name), 1, 0), "%s: simulated by the server everywhere" % b.name)
	for s: NetSession in [net.server_session, net.session(c)]:
		assert_eq(int(s.stats()["handoff_claims"]), 0, "no claims")
		assert_eq(int(s.stats()["handoff_held"]), 0, "nothing held")


# --- two claims in one tick ----------------------------------------------------------------------------

## The loser's copy freezes where it showed the body and blends to the winner's stream: the difference
## becomes a decaying offset (WP5's, about 15 % of it in the first frame). A claim lasts about a round
## trip (100 ms here), in which two pushers at 120 px/s move the copies at most 24 px apart, so no
## frame steps more than 15 % of that, 3.6 px, beyond the body's own motion (a crate is 20 px wide).
const BLEND_STEP := 0.15 * 2.0 * 120.0 * 0.1


## Two pushers walk into one crate from both sides on one key: client A's (or, with `host`, the server's
## own) from the left, client B's from the right. Returns the measurements.
func _dispute(f: NetFixture, host: bool) -> Dictionary:
	var managers := _managers(f)
	var world: Node = managers["Server"]
	var a: Node = f.clients[0]
	var b: Node = f.clients[1]
	var left_peer := 1 if host else f.peer_id(a)
	var sc: RigidBody2D = _add(world, CRATE, "Crate", Vector2(60, 290))
	_spawn_pusher(f, world, left_peer, "L", Vector2(0, 288), 1.0)
	_spawn_pusher(f, world, f.peer_id(b), "R", Vector2(120, 288), -1.0)
	var crates := await _copies(f, "World/Crate")
	await _copies(f, "World/R")
	var pr: CharacterBody2D = b.get_node("World/R")
	assert_true(await f.wait_until(func() -> bool: return pr.is_on_floor() and sc.sleeping, 5.0), "everything rests")
	await _frames(10)
	var copies: Array = [sc, crates[0], crates[1]]
	var server_claims0: int = f.server_session.stats()["handoff_claims"]
	Input.action_press("right")
	var xs: Array = [[], [], []]
	for t in 70:
		await get_tree().physics_frame
		for k in 3:
			xs[k].append((copies[k] as Node2D).position.x)
	Input.action_release("right")
	var settled := await f.wait_until(func() -> bool:
		var v := _views(f, "World/Crate")
		return v.all(func(x: Array) -> bool: return x == v[0]), 3.0)
	var views := _views(f, "World/Crate")
	assert_true(settled, "every peer agrees on the holder and the number: %s" % [views])
	var winner: int = views[0][0]
	var loser_branch := 2 if winner != f.peer_id(b) else 1
	var winner_copy := 0 if winner == 1 else (1 if winner == f.peer_id(a) else 2)
	# The loser's copy, frame by frame, beyond what the winner's copy moved in the same frame.
	var largest := 0.0
	var gap := 0.0
	for i in range(1, xs[0].size()):
		var own: float = absf(xs[loser_branch][i] - xs[loser_branch][i - 1])
		var ref: float = absf(xs[winner_copy][i] - xs[winner_copy][i - 1])
		largest = maxf(largest, own - ref)
		gap = maxf(gap, absf(xs[loser_branch][i] - xs[winner_copy][i]))
	var out := {
		"winner": winner,
		"seq": views[0][1],
		"claims": int(f.server_session.stats()["handoff_claims"]) - server_claims0,
		"refused_b": int(f.session(b).stats()["handoff_claims_refused"]),
		"largest": largest,
		"gap": gap,
		"loser_frozen": (copies[loser_branch] as RigidBody2D).freeze,
	}
	for n in ["L", "R", "Crate"]:
		world.get_node(n).queue_free()
	await _frames(5)
	return out


func test_two_claims_in_one_tick_agree_on_the_winner_and_the_loser_blends() -> void:
	timeout_sec = 30.0
	add_child(FLOOR.instantiate())
	var results := {}
	for host in [false, true]:
		net = NetFixture.new(self, 2, NetFixture.HostMode.HOST_PLAYER if host else NetFixture.HostMode.DEDICATED)
		for c in net.clients:
			net.set_emulation(c, _link(50.0))
		net.start()
		assert_true(await net.wait_connected())
		results[host] = await _dispute(net, host)
		results[host]["a"] = net.peer_id(net.clients[0])
		results[host]["b"] = net.peer_id(net.clients[1])
		net.teardown()
		net = null
	for host: bool in results:
		var r: Dictionary = results[host]
		print("handoff dispute (%s against client B, 50 ms each way): winner peer %d, sequence %d, %d claims at the server, B lost %d; the loser's copy stepped at most %.2f px beyond the winner's copy's own motion, at most %.2f px apart" % ["the host's player" if host else "client A", r["winner"], r["seq"], r["claims"], r["refused_b"], r["largest"], r["gap"]])
		assert_true(r["loser_frozen"], "the loser shows the body frozen")
		assert_lt(r["largest"], BLEND_STEP, "the loser blends: no step of %.0f px beyond the winner's motion" % BLEND_STEP)
	var d: Dictionary = results[false]
	assert_eq(d["winner"], d["a"], "two claims in one tick: the lower peer id (A) wins")
	assert_eq(d["seq"], 1, "one change of authority")
	assert_eq(d["refused_b"], 1, "B lost one claim")
	var h: Dictionary = results[true]
	assert_eq(h["winner"], 1, "the host's player touching it keeps it with the server")
	assert_eq(h["seq"], 0, "and nothing changed hands")
	assert_gt(h["refused_b"], 0, "B's claim was refused")


# --- late states, a holder leaving ------------------------------------------------------------------------


## The largest frame-to-frame step of `xs` beyond `speed` (px/s) over a 60 Hz frame.
func _excess(xs: Array, speed: float) -> float:
	var worst := 0.0
	for i in range(1, xs.size()):
		worst = maxf(worst, absf(float(xs[i]) - float(xs[i - 1])) - speed / 60.0)
	return worst


func test_late_states_of_a_previous_holder_are_dropped() -> void:
	timeout_sec = 15.0
	var managers := await _start(1)
	var world: Node = managers["Server"]
	var a: Node = net.clients[0]
	var sc: RigidBody2D = _add(world, CRATE, "Crate", Vector2(60, 290))
	_spawn_pusher(net, world, net.peer_id(a), "P", Vector2(0, 288), 1.0)
	await _copies(net, "World/P")
	var pa: CharacterBody2D = a.get_node("World/P")
	assert_true(await net.wait_until(func() -> bool: return pa.is_on_floor() and sc.sleeping, 5.0), "everything rests")
	Input.action_press("right")
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/Crate", net.peer_id(a), 1), 3.0), "A holds the crate it pushes")
	await _frames(20)
	# The server takes it back while A's states are on their way (50 ms each way, every tick).
	var stale0: int = net.server_session.stats()["handoff_stale_states"]
	Input.action_release("right")
	assert_true(Net.claim(sc), "the server takes the body")
	var xs: Array = []
	for t in 30:
		await get_tree().physics_frame
		xs.append(sc.position.x)
	var stale: int = int(net.server_session.stats()["handoff_stale_states"]) - stale0
	var jump := _excess(xs, 150.0)
	print("handoff, the server takes a body back from A over 50 ms each way: %d late states of A dropped; the server's copy stepped at most %.2f px beyond 150 px/s" % [stale, jump])
	assert_gt(stale, 0, "A's late states arrived and were dropped")
	assert_false(sc.freeze, "the server simulates it")
	assert_lt(jump, 0.5, "the server's copy never jumps back to a late state")
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/Crate", 1, 2), 3.0), "every peer: the server at sequence 2")


func test_a_holder_that_leaves_gives_the_body_back_at_once_still_moving() -> void:
	timeout_sec = 15.0
	var managers := await _start(2)
	var world: Node = managers["Server"]
	var results: Array = []
	for round in 2:
		# Round 0: the seat is held for the rejoin window; round 1: released at once.
		if round == 1:
			net.server_session.rejoin_timeout_s = 0.0
		var c: Node = net.clients[round]
		var sp: RigidBody2D = _add(world, PUCK, "Puck%d" % round, Vector2(0, 100 + 60 * round))
		assert_true(await net.wait_until(func() -> bool: return c.has_node("World/Puck%d" % round)), "the puck reached the client")
		var cp: RigidBody2D = c.get_node("World/Puck%d" % round)
		await _frames(5)
		assert_true(Net.claim(cp), "the client claims it")
		cp.linear_velocity = Vector2(150, 0)
		assert_true(await net.wait_until(func() -> bool: return sp.position.x > 20.0, 3.0), "the server's copy follows the holder")
		assert_true(sp.freeze, "frozen on the server while the client holds it")
		await _frames(5)
		var at := sp.position.x
		net.drop_client(round)
		var xs: Array = [at]
		var frames := -1
		for t in 20:
			await get_tree().physics_frame
			xs.append(sp.position.x)
			if frames < 0 and not sp.freeze:
				frames = t + 1
		var row := _row(net, net.server, sp)
		results.append({"frames": frames, "speed": sp.linear_velocity.x, "jump": _excess(xs, 150.0), "by": row["simulated_by"]})
	for round in 2:
		var r: Dictionary = results[round]
		print("handoff, the holder leaves (%s): the server simulates the body %d frame(s) later, at %.1f px/s, stepping at most %.2f px beyond 150 px/s" % ["seat held" if round == 0 else "seat released", r["frames"], r["speed"], r["jump"]])
		assert_between(r["frames"], 1, 3, "at once")
		assert_eq(int(r["by"]), 1, "simulated by the server")
		assert_almost_eq(r["speed"], 150.0, 2.0, "still moving at the holder's speed")
		assert_lt(r["jump"], 0.5, "from the holder's last state, without a jump")


# --- Net.claim and chains -------------------------------------------------------------------------------------


func test_a_claimed_item_is_held_until_released_and_a_held_body_claims_what_it_pushes() -> void:
	timeout_sec = 15.0
	var managers := await _start(1)
	var world: Node = managers["Server"]
	var a: Node = net.clients[0]
	var aid := net.peer_id(a)
	var item: RigidBody2D = _add(world, CRATE, "Item", Vector2(-60, 290))
	var c1: RigidBody2D = _add(world, CRATE, "C1", Vector2(60, 290))
	var c2: RigidBody2D = _add(world, CRATE, "C2", Vector2(140, 290))
	for n in ["Item", "C1", "C2"]:
		await _copies(net, "World/" + n)
	assert_true(await net.wait_until(func() -> bool: return item.sleeping and c1.sleeping and c2.sleeping, 5.0), "everything rests")
	# A carried item: game code claims it; it stays held at rest until released.
	var ia: RigidBody2D = a.get_node("World/Item")
	assert_true(Net.claim(ia), "A claims the item")
	assert_false(ia.freeze, "live on A at once")
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/Item", aid, 1), 2.0), "A holds it everywhere: %s" % [_views(net, "World/Item")])
	await wait_seconds(1.2)
	assert_true(_agree(net, "World/Item", aid, 1), "still A's after resting for twice the rest time")
	assert_true(Net.release_owner(ia), "released")
	assert_true(ia.freeze, "frozen on A at once")
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/Item", 1, 2), 2.0), "back with the server: %s" % [_views(net, "World/Item")])
	# A chain: a body A holds slides into another, which A then holds too.
	var a1: RigidBody2D = a.get_node("World/C1")
	assert_true(Net.claim(a1))
	a1.linear_velocity = Vector2(600, 0)
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/C2", aid, 1), 3.0), "A claimed the crate its crate hit: %s" % [_views(net, "World/C2")])
	var a2: RigidBody2D = a.get_node("World/C2")
	assert_true(await net.wait_until(func() -> bool: return a2.position.x > 150.0, 2.0), "A's crate pushed the second one along (it moved %.1f px)" % (a2.position.x - 140.0))
	assert_true(await net.wait_until(func() -> bool: return _agree(net, "World/C2", 1, 2), 4.0), "it comes back when it rests: %s" % [_views(net, "World/C2")])
	assert_true(await net.wait_until(func() -> bool: return absf(c2.position.x - a2.position.x) < SETTLED, 3.0), "the server has it where A left it (%s, A %s)" % [c2.position, a2.position])
	print("handoff chain: A's crate hit a second at 600 px/s, which A claimed ahead of the hit; it moved %.1f px" % (c2.position.x - 140.0))
	assert_true(_agree(net, "World/C1", aid, 1), "the claimed one stays A's")
	assert_gt(int(net.session(a).stats()["handoff_held"]), 0)


# --- body prediction on the other peers -------------------------------------------------------------------------


## Records, after gdnet wrote the frame's replicas, the x of the watched nodes and the time.
class Probe extends Node:
	var nodes: Array = []
	var xs: Array = []
	var times: Array = []

	func _init() -> void:
		process_priority = 1000

	func _process(_delta: float) -> void:
		if nodes.is_empty():
			return
		times.append(Time.get_ticks_usec())
		for i in nodes.size():
			xs[i].append((nodes[i] as Node2D).position.x)


func test_dead_reckoning_on_another_client_follows_the_holders_relayed_present() -> void:
	timeout_sec = 15.0
	var managers := await _start(2)
	var world: Node = managers["Server"]
	var a: Node = net.clients[0]
	var b: Node = net.clients[1]
	var sp: RigidBody2D = _add(world, PUCK, "Puck", Vector2(-100, 100))
	var copies := await _copies(net, "World/Puck")
	var pa: RigidBody2D = copies[0]
	var pb: RigidBody2D = copies[1]
	Net.configure(pb, {"body_prediction": "dead_reckoning"})
	await _frames(5)
	assert_true(Net.claim(pa))
	pa.linear_velocity = Vector2(SPEED, 0)
	assert_true(await net.wait_until(func() -> bool: return pb.position.x > 0.0, 3.0), "B shows it moving")
	assert_eq(Net.body_prediction_of(pb), NetEntity.BODY_PREDICTION_DEAD_RECKONING, "B dead-reckons A's body")
	assert_eq(Net.body_prediction_of(pa), NetEntity.BODY_PREDICTION_NONE, "never on its holder")
	var probe := Probe.new()
	add_child(probe)
	probe.xs = [[], []]
	probe.nodes = [pa, pb]
	await wait_seconds(0.6)
	probe.nodes = []
	# How long ago A's copy was where B shows it, per frame.
	var lags: Array = []
	var ax: Array = probe.xs[0]
	for i in range(ax.size()):
		var x: float = probe.xs[1][i]
		for j in range(1, ax.size()):
			if float(ax[j - 1]) <= x and x <= float(ax[j]) and float(ax[j]) > float(ax[j - 1]):
				var tc: float = float(probe.times[j - 1]) + (x - float(ax[j - 1])) / (float(ax[j]) - float(ax[j - 1])) * float(probe.times[j] - probe.times[j - 1])
				lags.append((float(probe.times[i]) - tc) / 1e6)
				break
	var lead: int = net.session(a).stats()["prediction_lead"]
	var bound := (lead + 3.0) / 60.0
	print("handoff body, dead reckoning on B at %.0f px/s over 50 ms each way: behind the holder's present by %.1f ms on average (%.1f..%.1f) over %d frames; A's prediction lead %d ticks, bound %.1f ms" % [SPEED, _mean(lags) * 1000.0, lags.min() * 1000.0, lags.max() * 1000.0, lags.size(), lead, bound * 1000.0])
	assert_gt(lags.size(), 20, "frames were measured")
	assert_lt(lags.max(), bound, "B shows the holder's relayed present: behind it by at most its lead plus three ticks")
	assert_gt(lags.min(), 0.0, "and never ahead of it")


const SPEED := 150.0


func _mean(a: Array) -> float:
	var s := 0.0
	for x: float in a:
		s += x
	return s / maxf(1.0, float(a.size()))
