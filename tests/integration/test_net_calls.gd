extends TestCase
## WP6: auto-exposed methods and the call verbs (docs/PLAN_NETCODE.md 1 rung 2, 2.1 "Auto-exposed
## methods", 2.4 `Call` / `CallReturn`, 3.1, D4, D14, gate G6). Headless over NetPipePeer.

const DOOR := preload("res://tests/fixtures/net/door.tscn")
const Door := preload("res://tests/fixtures/net/door.gd")
const PROBE := preload("res://tests/fixtures/net/call_probe.tscn")
const CALLING := preload("res://tests/fixtures/net/calling_player.tscn")
const RES_PATH := "res://addons/gdnet/default_profiles.tres"
const G6_SEED := 20261108

var net: NetFixture


func before_each() -> void:
	Door._set_allow_others(false)


func after_each() -> void:
	Input.action_release("jump")
	Door._set_allow_others(false)
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float, loss: float = 0.0, reorder: float = 0.0) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.send_loss = loss
	e.send_reorder = reorder
	e.receive_reorder = reorder
	e.seed = 11
	return e


func _start(clients: int, mode: NetFixture.HostMode = NetFixture.HostMode.HOST_PLAYER) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers()


## Spawns a door owned by `owner` on the server; returns its copies by branch name.
func _door(f: NetFixture, managers: Dictionary, owner: int, door_name: String = "Door") -> Dictionary:
	var d: Node = Net.spawn_for(owner, DOOR, managers["Server"], door_name)
	var out := {"Server": d}
	for c in f.clients:
		assert_true(await f.wait_until(func() -> bool: return c.has_node("World/" + door_name)), "the door reached " + String(c.name))
		out[String(c.name)] = c.get_node("World/" + door_name)
	return out


func _knocks(d: Node) -> Array:
	return d.calls.filter(func(c: Array) -> bool: return c[0] == "knock").map(func(c: Array) -> int: return c[1])


func test_rung2_a_clients_sync_runs_on_the_server_first_then_once_everywhere() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 2, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var a: Node = f.clients[0]
		var doors := await _door(f, managers, f.peer_id(a))
		var mine: Node = doors["ClientA"]
		var call: NetCall = Net.sync(mine.open_door, 7)
		assert_eq(mine.calls, [], "the caller does not run it first")
		assert_true(await f.wait_until(func() -> bool: return not mine.calls.is_empty()), "it came back to the caller")
		assert_eq(doors["Server"].calls, [["open_door", 7]], "the server ran it before the caller")
		assert_true(await f.wait_until(func() -> bool: return not doors["ClientB"].calls.is_empty()), "it reached the other client")
		await _frames(10)
		for k: String in doors:
			assert_eq(doors[k].calls, [["open_door", 7]], "exactly once on " + k)
			assert_true(doors[k].open, "open on " + k)
		assert_true(call.is_done() and call.ok, "the caller's NetCall resolved ok: %s" % call.error)
		var stats: Dictionary = f.session(a).stats()
		assert_eq(stats["calls_sent"], 1)
		assert_eq(f.server_session.stats()["calls_received"], 1))
	assert_true(ok)


func test_await_server_returns_the_value() -> void:
	var managers := await _start(1)
	var doors := await _door(net, managers, 1)
	var cdoor: Node = doors["ClientA"]
	var granted: bool = await Net.server(cdoor.try_buy, 3)
	assert_true(granted, "the server's answer reached the client")
	var refused: bool = await Net.server(cdoor.try_buy, 2)
	assert_false(refused)
	assert_eq(doors["Server"].calls, [["try_buy", 3], ["try_buy", 2]], "ran on the server only")
	assert_eq(cdoor.calls, [])
	var local: bool = await Net.server(doors["Server"].try_buy, 3)
	assert_true(local, "a server's own call runs locally and resolves too")
	var sig: Signal = Net.server(cdoor.try_buy, 3)
	var call: NetCall = sig.get_object()
	await sig
	assert_true(call.ok and call.result == true, "the signal's object is the NetCall")


func test_without_a_session_a_call_runs_locally() -> void:
	var before := NetSession.packets_sent_total()
	var door: Node = DOOR.instantiate()
	add_child(door)
	var call: NetCall = Net.sync(door.open_door, 4)
	assert_eq(door.calls, [["open_door", 4]], "the single-player path: it just runs")
	assert_true(call.ok)
	var v: bool = await Net.server(door.try_buy, 3)
	assert_true(v)
	assert_eq(NetSession.packets_sent_total(), before, "G0: nothing was sent")


func test_built_in_methods_and_bound_arguments_sync() -> void:
	var managers := await _start(1)
	var doors := await _door(net, managers, 1)
	var sdoor: Node = doors["Server"]
	var cdoor: Node = doors["ClientA"]
	Net.sync(sdoor.get_node("Timer").start, 2.0)
	Net.sync(sdoor.get_node("Label").set_text, "hello")
	Net.sync(sdoor.knock.bind(5))
	assert_true(await net.wait_until(func() -> bool: return _knocks(cdoor) == [5]), "the bound argument travelled")
	assert_false(cdoor.get_node("Timer").is_stopped(), "Timer.start ran on the client (a Timer has no replicated state)")
	assert_almost_eq(cdoor.get_node("Timer").wait_time, 2.0, 0.001)
	assert_eq(cdoor.get_node("Label").text, "hello")


func test_owner_others_and_peer_route_to_their_targets() -> void:
	var managers := await _start(2)
	var a: Node = net.clients[0]
	var b: Node = net.clients[1]
	var aid := net.peer_id(a)
	var bid := net.peer_id(b)
	var doors := await _door(net, managers, aid)
	var s: Node = doors["Server"]
	Net.owner(s.knock, 1)
	Net.others(s.knock, 2)
	Net.peer(bid, s.knock, 3)
	Net.others(doors["ClientA"].knock, 4)
	Net.peer(bid, doors["ClientA"].knock, 5)
	Net.peer(1, doors["ClientA"].knock, 6)
	Net.owner(doors["ClientA"].knock, 7)
	assert_true(await net.wait_until(func() -> bool: return _knocks(doors["ClientB"]).size() == 4))
	await _frames(10)
	assert_eq(_knocks(s), [4, 6], "the server: a client's others, a peer(1)")
	assert_eq(_knocks(doors["ClientA"]), [7, 1, 2], "the owner: its own owner call (local, at once), then owner, others")
	assert_eq(_knocks(doors["ClientB"]), [2, 3, 4, 5], "the other client: others, peer, others, peer")


func test_a_non_owners_sync_is_refused_and_net_allow_admits_it() -> void:
	var managers := await _start(2)
	var a: Node = net.clients[0]
	var bid := net.peer_id(net.clients[1])
	var doors := await _door(net, managers, net.peer_id(a))
	var refused := []
	net.server_session.call_refused.connect(func(node: Node, method: StringName, sender: int) -> void: refused.append([String(node.name), String(method), sender]))
	var bdoor: Node = doors["ClientB"]
	var call: NetCall = Net.sync(bdoor.open_door, 1)
	await call.completed
	assert_false(call.ok, "refused")
	assert_true(call.error.contains("refused"), call.error)
	assert_eq(refused, [["Door", "open_door", bid]], "call_refused on the server")
	assert_eq(doors["Server"].asked, [["open_door", bid]], "_net_allow was asked")
	await _frames(5)
	for k: String in doors:
		assert_eq(doors[k].calls, [], "nobody ran it on " + k)
	Door._set_allow_others(true)
	call = Net.sync(bdoor.open_door, 2)
	await call.completed
	assert_true(call.ok, call.error)
	assert_true(await net.wait_until(func() -> bool: return bdoor.calls == [["open_door", 2]]), "admitted, it ran on the caller too")
	assert_eq(doors["Server"].calls, [["open_door", 2]])


func test_net_call_info_carries_sender_and_tick() -> void:
	var managers := await _start(1, NetFixture.HostMode.DEDICATED)
	var cid := net.peer_id(net.clients[0])
	var doors := await _door(net, managers, cid)
	var s: Node = doors["Server"]
	var c: Node = doors["ClientA"]
	var tick := net.server_session.tick()
	Net.sync(s.note, "from server")
	assert_eq(s.infos.size(), 1, "ran at once on the server")
	assert_eq(s.infos[0], {"text": "from server", "sender": 1, "tick": tick, "is_local": true})
	assert_true(await net.wait_until(func() -> bool: return c.infos.size() == 1))
	assert_eq(c.infos[0], {"text": "from server", "sender": 1, "tick": tick, "is_local": false})
	var ctick := net.client_sessions[0].tick()
	Net.sync(c.note, "from owner")
	assert_true(await net.wait_until(func() -> bool: return c.infos.size() == 2))
	assert_eq(s.infos[1]["sender"], cid, "the server knows the sender")
	assert_eq(s.infos[1]["tick"], ctick, "and the tick it was sent at")
	assert_false(s.infos[1]["is_local"])
	assert_eq(c.infos[1]["sender"], cid)
	assert_true(c.infos[1]["is_local"], "the caller sees its own call as local")


func test_a_call_on_a_fresh_spawn_waits_behind_its_create() -> void:
	var managers := await _start(1)
	var client: Node = net.clients[0]
	var d: Node = DOOR.instantiate()
	d.name = "Fresh"
	managers["Server"].add_child(d)
	var call: NetCall = Net.sync(d.open_door, 9)   # the same tick as the spawn: no net id yet
	assert_false(call.is_done(), "it waits for the spawn: %s" % call.error)
	assert_true(await net.wait_until(func() -> bool: return client.has_node("World/Fresh") and not client.get_node("World/Fresh").calls.is_empty()))
	var replica: Node = client.get_node("World/Fresh")
	assert_eq(replica.calls_at_spawn, 0, "_net_spawned ran before the call")
	assert_eq(replica.calls, [["open_door", 9]])
	await _frames(5)
	assert_eq(d.calls, [["open_door", 9]], "once on the server too")


func test_a_call_inside_a_replayed_tick_is_suppressed() -> void:
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.set_emulation(net.clients[0], _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var cid := net.peer_id(net.clients[0])
	var sp: Node2D = Net.spawn_for(cid, CALLING, managers["Server"], "P")
	assert_true(await net.wait_until(func() -> bool: return net.clients[0].has_node("World/P")))
	var cp: Node2D = net.clients[0].get_node("World/P")
	await _frames(Engine.physics_ticks_per_second)
	Input.action_press("jump")
	sp.position.x += 50.0   # a correction whose rollback replays the tick of the press
	await _frames(4)
	Input.action_release("jump")
	assert_true(await net.wait_until(func() -> bool: return cp.jump_edges == 2 and cp.bumps == 1), "replayed, and the call came back once")
	await _frames(20)
	assert_eq(cp.sent, 1, "sent once")
	assert_eq(cp.suppressed, 1, "the replay's call was suppressed")
	assert_eq(cp.bumps, 1, "ran once on the owner")
	assert_eq(sp.bumps, 1, "ran once on the server")
	assert_eq(net.client_sessions[0].stats()["calls_sent"], 1)


func test_bad_calls_error_at_the_call_site() -> void:
	allow_errors = true
	var managers := await _start(1)
	var doors := await _door(net, managers, net.peer_id(net.clients[0]))
	var c: Node = doors["ClientA"]
	var sent: int = net.client_sessions[0].stats()["calls_sent"]
	var call: NetCall = Net.sync(c.knock, self)
	assert_false(call.ok)
	assert_true(call.error.contains("Object"), call.error)
	call = Net.sync(c.knock, Callable(self, "after_each"))
	assert_true(call.error.contains("Callable"), call.error)
	call = Net.sync(c._secret)
	assert_true(call.error.contains("beginning with _"), call.error)
	call = Net.sync(func() -> void: pass)
	assert_false(call.ok, "a lambda is not a method of a managed node")
	var stray: Node = DOOR.instantiate()
	net.clients[0].add_child(stray)   # in the session's branch, outside the manager
	call = Net.sync(stray.knock, 1)
	assert_true(call.error.contains("not part of a managed entity"), call.error)
	assert_eq(stray.calls, [], "not run either")
	assert_eq(net.client_sessions[0].stats()["calls_sent"], sent, "nothing was sent")
	await _frames(5)
	assert_eq(doors["Server"].calls, [])


func test_a_resource_with_a_path_travels_by_path() -> void:
	var managers := await _start(1)
	var doors := await _door(net, managers, 1)
	Net.sync(doors["Server"].take, load(RES_PATH))
	assert_true(await net.wait_until(func() -> bool: return doors["ClientA"].calls == [["take", RES_PATH]]))


func test_a_method_table_mismatch_names_the_scene_and_part() -> void:
	net = NetFixture.new(self, 1)
	net.client_sessions[0]._debug_extra_method = "Label:bogus"
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	managers["Server"].add_child(DOOR.instantiate())
	assert_true(await net.wait_until(func() -> bool: return net.disconnect_reasons.has("ClientA")), "the client was disconnected")
	var reason: String = net.disconnect_reasons["ClientA"]
	assert_true(reason.contains("method table of scene res://tests/fixtures/net/door.tscn differs at part Label"), reason)


## Gate G6: randomized spawn, reparent, call and destroy on the server under 30 ms latency, 10 % loss and
## reorder; the client never errors, every surviving entity ran its calls exactly once, in order, and
## the two sides hold the same entities.
func test_g6_no_ordering_race() -> void:
	print("G6 seed %d" % G6_SEED)
	var rng := RandomNumberGenerator.new()
	rng.seed = G6_SEED
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	net.set_emulation(net.clients[0], _link(30.0, 0.1, 0.1))
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var sroot: Node = managers["Server"]
	var croot: Node = managers["ClientA"]
	var expected := {}   # probe name -> keys fired at it
	var next_name := 0
	var key := 0
	var counts := {"spawn": 0, "same_tick": 0, "reparent": 0, "destroy": 0, "call": 0}
	for step in 200:
		var alive := _probes(sroot)
		var roll := rng.randf()
		if roll < 0.3 and alive.size() < 12:
			var p: Node2D = PROBE.instantiate()
			p.name = "P%d" % next_name
			next_name += 1
			var parent: Node = sroot
			if not alive.is_empty() and rng.randf() < 0.4:
				parent = alive[rng.randi() % alive.size()].get_node("Slot")
			parent.add_child(p)
			expected[String(p.name)] = []
			counts["spawn"] += 1
			if rng.randf() < 0.5:
				key += 1
				Net.sync(p.hit, key)
				expected[String(p.name)].append(key)
				counts["same_tick"] += 1
		elif roll < 0.45 and alive.size() > 1:
			var p: Node = alive[rng.randi() % alive.size()]
			var targets: Array[Node] = [sroot]
			for q in alive:
				if q != p and not p.is_ancestor_of(q):
					targets.append(q.get_node("Slot"))
			var t: Node = targets[rng.randi() % targets.size()]
			if t != p.get_parent():
				p.reparent(t)
				counts["reparent"] += 1
		elif roll < 0.55 and not alive.is_empty():
			alive[rng.randi() % alive.size()].queue_free()
			counts["destroy"] += 1
		elif not alive.is_empty():
			for i in rng.randi_range(1, 3):
				var p: Node = alive[rng.randi() % alive.size()]
				key += 1
				Net.sync(p.hit, key)
				expected[String(p.name)].append(key)
				counts["call"] += 1
		await get_tree().physics_frame
	var settled := await net.wait_until(func() -> bool: return _g6_matches(sroot, croot), 8.0)
	var survivors := _probes(sroot)
	print("G6: %s; %d survivors; client stats %s" % [counts, survivors.size(), _call_stats(net.client_sessions[0].stats())])
	assert_true(settled, "the client converged on the server's entities and calls")
	assert_eq(_probes(croot).size(), survivors.size(), "no dangling replica")
	for p in survivors:
		var rel := sroot.get_path_to(p)
		var cp: Node = croot.get_node_or_null(rel)
		assert_not_null(cp, "the client has %s" % rel)
		assert_eq(p.hits, expected[String(p.name)], "the server ran %s's calls once each" % p.name)
		if cp != null:
			assert_eq(cp.hits, p.hits, "the client ran %s's calls once each, in order" % p.name)


func _probes(root: Node) -> Array[Node]:
	var out: Array[Node] = []
	for n in root.find_children("P*", "Node2D", true, false):
		if n.has_method("hit") and not n.is_queued_for_deletion() and n.is_inside_tree():
			var dying := false
			var cur: Node = n
			while cur != root and cur != null:
				if cur.is_queued_for_deletion():
					dying = true
					break
				cur = cur.get_parent()
			if not dying:
				out.append(n)
	return out


func _g6_matches(sroot: Node, croot: Node) -> bool:
	var s := _probes(sroot)
	if _probes(croot).size() != s.size():
		return false
	for p in s:
		var cp: Node = croot.get_node_or_null(sroot.get_path_to(p))
		if cp == null or cp.hits != p.hits:
			return false
	return true


func _call_stats(d: Dictionary) -> Dictionary:
	var out := {}
	for k: String in d:
		if k.begins_with("calls_"):
			out[k] = d[k]
	return out
