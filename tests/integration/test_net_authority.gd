extends TestCase
## WP7: authority and keyed state (docs/PLAN_NETCODE.md 1 rung 2, 2.1 "Who simulates" and "Spawn", 2.6,
## 3.1, gates G0 and G2): owners inherited by nested entities, owner changes moving simulation, the
## transfer rules `take` / `request` / `fixed` and release, `client_owned` entities simulated by their
## owner and relayed by the server, rejected writes rolled back on the writer, and `Net.state` with its
## permissions and views. Headless over NetPipePeer.

const CRATE := preload("res://tests/fixtures/net/crate.tscn")
const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const OWNED_PLAYER := preload("res://tests/fixtures/net/owned_player.tscn")
const BOX := preload("res://tests/fixtures/net/owned_box.tscn")
const NESTED := preload("res://tests/fixtures/net/owned_nested.tscn")
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]

var net: NetFixture


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	if net != null:
		net.teardown()
		net = null
	Net.leave()


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 7
	return e


## A connected fixture with managers named `World` (`policy` is the managers' spawn_policy).
func _start(clients: int, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED, ms: float = 0.0, policy: int = 0) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	if ms > 0.0:
		for c in net.clients:
			net.set_emulation(c, _link(ms))
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers("World", func(m: NetManager) -> void: m.spawn_policy = policy)


## Adds `node` (named `node_name`, owned by `owner`, with `transfer`) under the server's manager and
## returns its copies by branch name once every client has it.
func _spawn(f: NetFixture, managers: Dictionary, scene: PackedScene, node_name: String, owner: int, transfer: int = 0) -> Dictionary:
	var node: Node = scene.instantiate()
	node.name = node_name
	var ne: NetEntity = node.get_node_or_null("NetEntity")
	if ne != null:
		ne.transfer = transfer
	Net.set_owner(node, owner)
	managers["Server"].add_child(node)
	var out := {"Server": node}
	for c in f.clients:
		var path := "World/" + node_name
		assert_true(await f.wait_until(func() -> bool: return c.has_node(path) and Net.net_id(c.get_node(path)) > 0), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	return out


func _owners(copies: Dictionary) -> Array:
	var out := []
	for k: String in copies:
		out.append(Net.get_owner_of(copies[k]))
	return out


func test_a_nested_entity_inherits_its_owner_unless_it_opts_out() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var client: Node = f.clients[0]
		var cid := f.peer_id(client)
		Net.spawn_for(cid, NESTED, managers["Server"], "N")
		assert_true(await f.wait_until(func() -> bool:
			return client.has_node("World/N/Keep") and Net.net_id(client.get_node("World/N/Keep")) > 0 \
				and Net.net_id(client.get_node("World/N/Inherit")) > 0), "the nested entities arrived")
		for branch: Node in [f.server, client]:
			var n: Node = branch.get_node("World/N")
			assert_eq(Net.get_owner_of(n), cid, "the root belongs to the client (%s)" % branch.name)
			assert_eq(Net.get_owner_of(n.get_node("Inherit")), cid, "a nested entity inherits it (%s)" % branch.name)
			assert_eq(Net.get_owner_of(n.get_node("Keep")), 1, "inherit_owner = false keeps the default (%s)" % branch.name)
			assert_eq((n.get_node("Inherit/NetEntity") as NetEntity).owner_peer, cid)
		assert_true(Net.is_owner(client.get_node("World/N/Inherit"))))
	assert_true(ok)


func test_set_owner_moves_prediction_and_fires_owner_changed_once() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	var b := net.peer_id(net.clients[1])
	var copies := await _spawn(net, managers, OWNED_PLAYER, "P", a)
	var signals := {}
	for k: String in copies:
		var list: Array = []
		signals[k] = list
		(copies[k].get_node("NetEntity") as NetEntity).owner_changed.connect(func(old: int, new: int) -> void: list.append([old, new]))
	var relayed := []
	net.server_session.owner_changed.connect(func(n: Node, old: int, new: int) -> void: relayed.append([n.name, old, new]))
	await _frames(Engine.physics_ticks_per_second / 2)
	assert_true(copies["ClientA"].is_physics_processing(), "the owner simulates")
	assert_true(Net.set_owner(copies["Server"], b))
	assert_true(await net.wait_until(func() -> bool:
		return copies["ClientB"].is_physics_processing() and not copies["ClientA"].is_physics_processing()), "B simulates now, A is a replica")
	await _frames(10)
	for k: String in copies:
		assert_eq(signals[k], [[a, b]], "owner_changed fired once on " + k)
		assert_eq(copies[k].owner_changes, [[a, b]], "_net_owner_changed ran once on " + k)
	assert_eq(relayed, [[&"P", a, b]], "the session relays it once")
	assert_eq(_owners(copies), [b, b, b])
	await _frames(Engine.physics_ticks_per_second / 2)
	var start: Vector2 = copies["ClientB"].position
	Input.action_press("right")
	await _frames(20)
	Input.action_release("right")
	assert_gt(copies["ClientB"].position.x, start.x + 10.0, "B's presses move it")
	assert_true(await net.wait_until(func() -> bool:
		return copies["Server"].position.distance_to(copies["ClientB"].position) < 0.01 \
			and copies["ClientA"].position.distance_to(copies["ClientB"].position) < 0.5), "the server and A follow B")


func test_take_moves_the_entity_to_the_taker() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	var copies := await _spawn(net, managers, PLAYER, "T", 1, 2)
	assert_eq((copies["ClientA"].get_node("NetEntity") as NetEntity).transfer, 2, "the rule reached the client")
	var call: NetCall = Net.request_owner(copies["ClientA"])
	var granted: Variant = await call.completed
	assert_eq(granted, true, "take is always granted")
	assert_true(call.ok)
	assert_true(await net.wait_until(func() -> bool: return _owners(copies) == [a, a, a] and copies["ClientA"].is_physics_processing()))
	await _frames(Engine.physics_ticks_per_second / 2)
	var start: Vector2 = copies["ClientB"].position
	Input.action_press("right")
	await _frames(20)
	Input.action_release("right")
	assert_true(await net.wait_until(func() -> bool:
		return copies["ClientB"].position.x > start.x + 10.0 \
			and copies["ClientB"].position.distance_to(copies["ClientA"].position) < 0.5), "the other client sees the taker move it")


func test_request_fixed_and_release() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	var b := net.peer_id(net.clients[1])
	var server_owned := await _spawn(net, managers, PLAYER, "R", 1, 1)
	var hooked := await _spawn(net, managers, OWNED_PLAYER, "H", a, 1)
	var fixed := await _spawn(net, managers, PLAYER, "F", 1, 0)
	# request, server-owned: granted by default.
	assert_eq(await Net.request_owner(server_owned["ClientA"]).completed, true)
	assert_true(await net.wait_until(func() -> bool: return _owners(server_owned) == [a, a, a]))
	# request, client-owned, no hook: the owner refuses by default.
	assert_eq(await Net.request_owner(server_owned["ClientB"]).completed, false)
	await _frames(5)
	assert_eq(_owners(server_owned), [a, a, a], "still A's")
	# request, client-owned, the owner's _net_grant_owner says yes.
	hooked["ClientA"].grant = true
	assert_eq(await Net.request_owner(hooked["ClientB"]).completed, true)
	assert_eq(hooked["ClientA"].asked_by, [b], "the owner was asked, by B")
	assert_true(await net.wait_until(func() -> bool: return _owners(hooked) == [b, b, b]))
	# fixed: refused.
	assert_eq(await Net.request_owner(fixed["ClientA"]).completed, false)
	await _frames(5)
	assert_eq(_owners(fixed), [1, 1, 1])
	# release: back to the server.
	assert_true(Net.release_owner(server_owned["ClientA"]))
	assert_true(await net.wait_until(func() -> bool: return _owners(server_owned) == [1, 1, 1]), "released")
	assert_false(Net.release_owner(fixed["ClientA"]), "only the owner releases")
	assert_gt(net.server_session.stats()["ownership_requests_refused"], 1)


func test_a_client_owned_box_moved_by_its_owner_reaches_everyone_without_echo() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var managers := await _start(2, mode, 30.0)
		var a := net.peer_id(net.clients[0])
		var copies := await _spawn(net, managers, BOX, "Box", a)
		var owner_copy: Node2D = copies["ClientA"]
		await _frames(5)
		assert_true(owner_copy.is_physics_processing(), "the owner simulates it")
		assert_false(copies["Server"].is_physics_processing(), "the server does not")
		assert_false(copies["ClientB"].is_physics_processing(), "another client does not")
		owner_copy.seen.clear()
		owner_copy.speed = Vector2(120, 0)
		await _frames(60)
		var steps := 0
		for i in range(1, owner_copy.seen.size()):
			var d: float = owner_copy.seen[i].x - owner_copy.seen[i - 1].x
			assert_almost_eq(d, 2.0, 0.001, "the owner's copy moves by its own script only, frame %d" % i)
			steps += 1
		assert_gt(steps, 50)
		assert_true(await net.wait_until(func() -> bool: return copies["Server"].position.x > 60.0 and copies["ClientB"].position.x > 60.0), "the server and the other client see it move")
		owner_copy.speed = Vector2.ZERO
		assert_true(await net.wait_until(func() -> bool:
			return copies["Server"].position.distance_to(owner_copy.position) < 0.01 \
				and copies["ClientB"].position.distance_to(owner_copy.position) < 0.5), "they settle on the owner's position")
		assert_gt(net.server_session.stats()["owner_states_received"], 0)
		assert_eq(net.client_sessions[0].stats()["writes_rejected"], 0)
		net.teardown()
		net = null


func test_an_owner_allowed_spawn_moved_by_its_owner_reaches_the_other_client() -> void:
	var managers := await _start(2, NetFixture.HostMode.DEDICATED, 0.0, 1)
	var c: Node2D = CRATE.instantiate()
	c.name = "Mine"
	managers["ClientA"].add_child(c)
	var b: Node = net.clients[1]
	assert_true(await net.wait_until(func() -> bool: return b.has_node("World/Mine")))
	var other: Node2D = b.get_node("World/Mine")
	for i in 30:
		c.position.x += 3.0
		await get_tree().physics_frame
	assert_true(await net.wait_until(func() -> bool:
		return other.position.distance_to(c.position) < 0.5 and net.server.get_node("World/Mine").position.distance_to(c.position) < 0.01), "the owner's movement is relayed (%s vs %s)" % [other.position, c.position])


func test_a_write_to_an_entity_the_client_does_not_own_is_rolled_back() -> void:
	var managers := await _start(1)
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(net, managers, CRATE, "W", 1)
	var replica: Node2D = copies["ClientA"]
	var on_server := []
	var on_client := []
	net.server_session.write_rejected.connect(func(n: Node, sender: int) -> void: on_server.append([n.name, sender]))
	net.client_sessions[0].write_rejected.connect(func(n: Node, sender: int) -> void: on_client.append([n.name, sender]))
	await _frames(3)
	replica.z_index = 7
	assert_true(await net.wait_until(func() -> bool: return on_client.size() == 1), "the writer hears of the rejection")
	assert_eq(replica.z_index, 0, "the local value snapped back to the server's")
	assert_eq(on_client, [[&"W", cid]])
	assert_eq(on_server, [[&"W", cid]], "the server names the writer")
	assert_eq(copies["Server"].z_index, 0, "the server's value never changed")
	await _frames(12)
	assert_eq(on_client.size(), 1, "rejected once")
	assert_eq(net.server_session.stats()["writes_rejected"], 1)


func test_keyed_state_reaches_clients_and_a_late_joiner() -> void:
	net = NetFixture.new(self, 2)
	net.start_server()
	var a: Node = net.clients[0]
	net.client_sessions[0].start(net.peer(a), a.get_path())
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].is_online()))
	var changes := []
	net.client_sessions[0].state.changed.connect(func(path: String, value: Variant) -> void: changes.append([path, value]))
	var ss: NetKeyedState = net.server_session.state
	ss.set("score/1", 10)
	ss.set("title", "Doors")
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].state.get("score/1") == 10))
	ss.set("score/1", 11)
	ss.erase("title")
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].state.get("score/1") == 11 and not net.client_sessions[0].state.has("title")))
	assert_eq(changes, [["score/1", 10], ["title", "Doors"], ["score/1", 11]])
	assert_eq(net.client_sessions[0].state.get("missing", "fallback"), "fallback")
	net.start_clients()
	assert_true(await net.wait_connected())
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[1].state.get("score/1") == 11), "the late joiner has the score")
	assert_eq(net.client_sessions[1].state.keys(), PackedStringArray(["score/1"]))
	assert_gt(net.server_session.stats()["keyed_bytes_sent"], 0)


func test_keyed_permissions() -> void:
	allow_errors = true   # the refused write prints its reason
	await _start(2)
	var sa: NetKeyedState = net.client_sessions[0].state
	var sb: NetKeyedState = net.client_sessions[1].state
	var ss: NetKeyedState = net.server_session.state
	assert_false(sa.set("chat/1", "too early"), "server-only by default: refused locally")
	ss.allow("chat/", "all")
	ss.allow("score/", "owner")
	await _frames(3)
	assert_true(sa.set("chat/1", "hi"))
	assert_eq(sa.get("chat/1"), "hi", "applied at once on the writer")
	assert_true(await net.wait_until(func() -> bool: return sb.get("chat/1") == "hi" and ss.get("chat/1") == "hi"), "the others read it")
	var a := net.peer_id(net.clients[0])
	assert_true(sa.set("score/%d" % a, 5), "its own score path")
	assert_false(sa.set("score/1", 5), "someone else's path is refused")
	assert_false(sa.set("admin", 1), "outside every allowance")
	await _frames(5)
	assert_false(ss.has("admin"))
	assert_false(ss.has("score/1"))
	assert_eq(ss.get("score/%d" % a), 5)


func test_a_view_hides_a_path_from_one_client() -> void:
	await _start(2)
	var a := net.peer_id(net.clients[0])
	var ss: NetKeyedState = net.server_session.state
	ss.view("team/red/", func(peer: int) -> bool: return peer == a)
	ss.set("team/red/plan", "flank")
	ss.set("open", 1)
	assert_true(await net.wait_until(func() -> bool: return net.client_sessions[0].state.get("team/red/plan") == "flank" and net.client_sessions[1].state.has("open")))
	await _frames(10)
	assert_false(net.client_sessions[1].state.has("team/red/plan"), "B never sees it")


func test_g0_keyed_state_without_a_session_is_local_and_sends_nothing() -> void:
	var before := NetSession.packets_sent_total()
	var seen := []
	var cb := func(path: String, value: Variant) -> void: seen.append([path, value])
	Net.state.changed.connect(cb)
	assert_true(Net.state.set("wp7/local", 3))
	assert_eq(Net.state.get("wp7/local"), 3)
	assert_eq(seen, [["wp7/local", 3]])
	assert_true(Net.state.erase("wp7/local"))
	Net.state.changed.disconnect(cb)
	await _frames(3)
	assert_eq(NetSession.packets_sent_total(), before, "no packets")
	var lone := Node2D.new()
	var call: NetCall = Net.request_owner(lone)
	assert_true(call.is_done() and call.result == true, "single player: everything is yours")
	lone.free()
