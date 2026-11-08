extends TestCase
## WP4: the input map as input, `_physics_process` as the tick, prediction and rollback
## (docs/PLAN_NETCODE.md 1 rung 1, 2.1 "Who simulates" and "Input", 2.5, gates G2 and G5).
##
## Every scenario drives the real `Input` with `Input.action_press` / `action_release`: the owning
## client's manager samples it into the player's input frame (all branches share one process, so the
## server branch never reads it for a client's player: it uses the frames the client sent). The link is
## 50 ms each way (a 100 ms round trip) unless a test says otherwise.

const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const EVERYWHERE := preload("res://tests/fixtures/net/predicted_player_everywhere.tscn")
const UNPREDICTED := preload("res://tests/fixtures/net/predicted_player_unpredicted.tscn")
const TICK_PLAYER := preload("res://tests/fixtures/net/net_tick_player.tscn")
const GLOBAL_INPUT := preload("res://tests/fixtures/net/global_input_player.tscn")
const AIM := preload("res://tests/fixtures/net/aim_player.tscn")
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]

var net: NetFixture
var _rate: int = 0
var _rolled: Dictionary = {}


func before_each() -> void:
	_rate = Engine.physics_ticks_per_second
	_rolled = {}


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	Net.input_delay_ticks = 0
	if net != null:
		net.teardown()
		net = null
	Engine.physics_ticks_per_second = _rate


func _link(ms: float, loss: float = 0.0) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.send_loss = loss
	e.seed = 7
	return e


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## Starts a fixture with `clients` clients over the 50 ms link, managers under `World`.
func _start(clients: int = 1, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED, ms: float = 50.0) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	for c in net.clients:
		net.set_emulation(c, _link(ms))
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers()


## Spawns `scene` for `peer` under the server's manager, waits for every client's copy and for the
## prediction lead to settle (a second of pings). Returns the copies by branch name.
func _spawn(managers: Dictionary, scene: PackedScene, peer: int, node_name: String = "P") -> Dictionary:
	var sp: Node = Net.spawn_for(peer, scene, managers["Server"], node_name)
	var out := {"Server": sp}
	for c in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path)), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	await _frames(Engine.physics_ticks_per_second)
	for k: String in out:
		var ne: Node = out[k].get_node_or_null("NetEntity")
		_rolled[k] = []
		if ne != null:
			var list: Array = _rolled[k]
			ne.rolled_back.connect(func(from_tick: int, to_tick: int) -> void: list.append([from_tick, to_tick]))
	return out


## Ticks until `a` and `b` are within 1 mm (or `limit`); returns -1 when they never met.
func _ticks_to_converge(a: Node2D, b: Node2D, limit: int) -> int:
	for t in limit:
		if a.position.distance_to(b.position) < 0.001:
			return t
		await get_tree().physics_frame
	return -1


## The rung 1 scenario: the owner moves on the very next tick, the server follows, they converge.
## Returns the ticks the server needed after the release to reach the owner's position.
func _move_and_converge(sp: Node2D, cp: Node2D) -> int:
	var start := cp.position
	assert_eq(sp.position, start, "both copies start in the same place")
	await get_tree().physics_frame
	Input.action_press("right")
	await get_tree().physics_frame
	assert_gt(cp.position.x, start.x, "the owner moved on the very tick after the press (prediction)")
	assert_eq(sp.position.x, start.x, "the server has not received the input yet")
	await _frames(Engine.physics_ticks_per_second / 2)
	assert_gt(sp.position.x, start.x, "the server's copy follows")
	Input.action_release("right")
	await get_tree().physics_frame
	var n := await _ticks_to_converge(sp, cp, Engine.physics_ticks_per_second * 2)
	assert_ne(n, -1, "the copies converged (server %s, owner %s)" % [sp.position, cp.position])
	var expected := start.x + 200.0 * float(Engine.physics_ticks_per_second / 2 + 1) / Engine.physics_ticks_per_second
	assert_almost_eq(cp.position.x, expected, 200.0 / Engine.physics_ticks_per_second * 1.5, "the owner moved for as long as the key was held")
	return n


func test_owner_predicts_and_converges_with_the_server() -> void:
	# Gate G2: once with the host as a player, once as a dedicated server.
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var managers := await _start(1, mode)
		var cid := net.peer_id(net.clients[0])
		var copies := await _spawn(managers, PLAYER, cid)
		var n := await _move_and_converge(copies["Server"], copies["ClientA"])
		print("converged %d ticks after the release (mode %d)" % [n, mode])
		assert_lt(n, 30, "within 30 ticks")
		assert_eq(_rolled["ClientA"].size(), 0, "no rollback while the prediction is right")
		assert_eq(copies["ClientA"].replayed_calls, 0)
		net.teardown()
		net = null


func test_g5_rate_independence() -> void:
	for rate: int in [30, 60, 120]:
		Engine.physics_ticks_per_second = rate
		await _frames(2)
		var managers := await _start()
		var cid := net.peer_id(net.clients[0])
		var copies := await _spawn(managers, PLAYER, cid)
		var sp: Node2D = copies["Server"]
		var cp: Node2D = copies["ClientA"]
		var n := await _move_and_converge(sp, cp)
		assert_eq(_rolled["ClientA"].size(), 0, "no rollback at %d Hz" % rate)
		# A correction from the server is still caught at this rate.
		sp.position.y += 50.0
		var m := await _ticks_to_converge(sp, cp, rate * 2)
		assert_ne(m, -1, "the owner took the server's correction at %d Hz" % rate)
		await _frames(rate / 4)
		assert_eq(_rolled["ClientA"].size(), 1, "one rollback at %d Hz" % rate)
		var stats: Dictionary = net.client_sessions[0].stats()
		print("G5 %d Hz: converged %d ticks after the release, %d after an impulse; lead %d ticks; rollbacks %d; replayed %d; stale %d" % [rate, n, m, stats["prediction_lead"], stats["rollbacks"], stats["replayed_ticks"], stats["prediction_stale"]])
		net.teardown()
		net = null
		await _frames(2)


func test_a_server_impulse_rolls_back_once_and_converges() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, PLAYER, cid)
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	await _move_and_converge(sp, cp)
	assert_eq(_rolled["ClientA"].size(), 0)
	sp.position.x += 50.0
	var n := await _ticks_to_converge(sp, cp, 120)
	assert_ne(n, -1, "the owner took the server's correction")
	await _frames(30)
	assert_eq(_rolled["ClientA"].size(), 1, "exactly one rollback: %s" % [_rolled["ClientA"]])
	assert_eq(_rolled["Server"].size(), 0, "the server never rolls back")
	var r: Array = _rolled["ClientA"][0]
	assert_gt(r[0], r[1], "rolled back from the predicted tick to the confirmed one")
	assert_lt(cp.position.distance_to(sp.position), 0.001, "converged within 1 mm")
	var stats: Dictionary = net.client_sessions[0].stats()
	assert_eq(stats["rollbacks"], 1)
	assert_gt(stats["replayed_ticks"], 0)
	print("impulse: converged %d ticks after the teleport; replayed %d ticks" % [n, stats["replayed_ticks"]])


func test_a_press_fires_once_on_each_side_across_a_replay() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, PLAYER, cid)
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	await get_tree().physics_frame
	Input.action_press("jump")
	# A server impulse in the same tick: the rollback it causes replays the tick of the press.
	sp.position.x += 50.0
	await _frames(4)
	Input.action_release("jump")
	assert_ne(await _ticks_to_converge(sp, cp, 120), -1)
	await _frames(20)
	assert_eq(_rolled["ClientA"].size(), 1, "one rollback")
	assert_gt(cp.replayed_calls, 0, "the replay ran _physics_process again")
	assert_eq(cp.jump_edges, 2, "the replay saw the press again")
	assert_eq(cp.jumps, 1, "the is_fresh one-shot ran once on the owner")
	assert_eq(sp.jumps, 1, "just-pressed fired once on the server")
	assert_eq(sp.jump_edges, 1)


func test_replicas_do_not_simulate_and_everywhere_opts_out() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, PLAYER, a, "P")
	var other := await _spawn(managers, EVERYWHERE, a, "Q")
	await _frames(10)
	var replica: Node = copies["ClientB"]
	assert_eq(replica.physics_calls, 0, "the replica's _physics_process does not run")
	assert_gt(replica.process_calls, 0, "its _process does")
	assert_false(replica.is_physics_processing())
	assert_gt(copies["ClientA"].physics_calls, 0, "the owner simulates")
	assert_gt(copies["Server"].physics_calls, 0, "the server simulates")
	assert_gt(other["ClientB"].physics_calls, 0, "simulate = everywhere keeps it running on a replica")
	# The replica still follows the server's state.
	Input.action_press("right")
	await _frames(30)
	Input.action_release("right")
	assert_true(await net.wait_until(func() -> bool: return replica.position.distance_to(copies["Server"].position) < 0.01 and copies["Server"].position.x > 0.0), "the replica follows")


func test_ownership_change_moves_simulation() -> void:
	var managers := await _start(2)
	var a := net.peer_id(net.clients[0])
	var b := net.peer_id(net.clients[1])
	var copies := await _spawn(managers, PLAYER, a)
	assert_true(copies["ClientA"].is_physics_processing())
	assert_false(copies["ClientB"].is_physics_processing())
	assert_true(Net.set_owner(copies["Server"], b))
	assert_true(await net.wait_until(func() -> bool: return copies["ClientB"].is_physics_processing() and not copies["ClientA"].is_physics_processing()), "B simulates now, A stopped")
	await _frames(Engine.physics_ticks_per_second)
	var n := await _move_and_converge(copies["Server"], copies["ClientB"])
	assert_lt(n, 30)


func test_the_hosts_own_player_never_rolls_back() -> void:
	var managers := await _start(1, NetFixture.HostMode.HOST_PLAYER)
	var copies := await _spawn(managers, PLAYER, 1)
	var sp: Node2D = copies["Server"]
	var rp: Node2D = copies["ClientA"]
	assert_true(Net.input(sp).is_live(), "the host's own player reads the real Input")
	await get_tree().physics_frame
	Input.action_press("right")
	await get_tree().physics_frame
	assert_gt(sp.position.x, 0.0, "the host moves at once")
	await _frames(20)
	Input.action_release("right")
	sp.position.y += 50.0
	assert_true(await net.wait_until(func() -> bool: return rp.position.distance_to(sp.position) < 0.01), "the replica follows")
	await _frames(10)
	assert_eq(_rolled["Server"].size(), 0)
	assert_eq(_rolled["ClientA"].size(), 0)
	assert_eq(net.server_session.stats()["rollbacks"], 0)
	assert_eq(net.client_sessions[0].stats()["rollbacks"], 0)
	assert_eq(rp.physics_calls, 0, "the client only shows it")


func test_net_tick_takes_precedence() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, TICK_PLAYER, cid)
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	assert_gt(sp.net_ticks, 0, "the server calls _net_tick")
	assert_gt(cp.fresh_ticks, 0, "the owner calls _net_tick")
	assert_eq(sp.physics_calls, 0, "_physics_process is disabled where _net_tick runs")
	assert_eq(cp.physics_calls, 0)
	assert_false(sp.is_physics_processing())
	var n := await _move_and_converge(sp, cp)
	assert_lt(n, 30)
	assert_eq(sp.last_tick, net.server_session.tick(), "the server's _net_tick gets its tick")


func test_input_delay_delays_the_owner_by_two_ticks() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, PLAYER, cid)
	var cp: Node2D = copies["ClientA"]
	Net.input_delay_ticks = 2
	await _frames(5)
	assert_eq(net.client_sessions[0].stats()["input_delay"], 2)
	var x := cp.position.x
	await get_tree().physics_frame
	Input.action_press("right")
	await get_tree().physics_frame
	assert_eq(cp.position.x, x, "not on the next tick")
	await get_tree().physics_frame
	assert_eq(cp.position.x, x, "not on the tick after")
	await get_tree().physics_frame
	assert_gt(cp.position.x, x, "two ticks later than without delay")
	await _frames(10)
	Input.action_release("right")
	assert_ne(await _ticks_to_converge(copies["Server"], cp, 120), -1, "and still converges")


func test_predict_disabled_never_rolls_back() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, UNPREDICTED, cid)
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	assert_false(cp.is_physics_processing(), "an owner that does not predict shows the server's state")
	Input.action_press("right")
	await _frames(20)
	Input.action_release("right")
	sp.position.x += 50.0
	assert_true(await net.wait_until(func() -> bool: return cp.position.distance_to(sp.position) < 0.01 and sp.position.x > 50.0), "the owner follows the server")
	await _frames(10)
	assert_eq(_rolled["ClientA"].size(), 0)
	assert_eq(net.client_sessions[0].stats()["rollbacks"], 0)


func test_global_input_in_an_owned_entity_warns_once() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	await _spawn(managers, PLAYER, cid, "Good")
	assert_false(Net.global_input_warnings().has("res://tests/fixtures/net/predicted_player.gd"), "Net.input is fine")
	await _spawn(managers, GLOBAL_INPUT, cid, "Bad")
	assert_true(Net.global_input_warnings().has("res://tests/fixtures/net/global_input_player.gd"), "Input.get_vector in the tick warns")


func test_two_of_three_inputs_lost_do_not_stall_the_server() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	net.set_emulation(net.clients[0], _link(50.0, 0.66))
	var copies := await _spawn(managers, PLAYER, cid)
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	var start := sp.position.x
	Input.action_press("right")
	var stalls := 0
	var last := sp.position.x
	await _frames(net.client_sessions[0].stats()["prediction_lead"] + 6)
	for i in 60:
		await get_tree().physics_frame
		if sp.position.x <= last:
			stalls += 1
		last = sp.position.x
	Input.action_release("right")
	var stats: Dictionary = net.server_session.stats()
	print("2-of-3 loss: server moved %.1f px in 60 ticks, %d stalled ticks, %d repeated frames" % [sp.position.x - start, stalls, stats["inputs_repeated"]])
	assert_gt(sp.position.x - start, 150.0, "the server's copy kept moving")
	assert_lt(stalls, 10, "redundancy covered most losses")
	assert_ne(await _ticks_to_converge(sp, cp, 240), -1, "and the copies converge")


func test_custom_values_and_input_properties_reach_the_server() -> void:
	var managers := await _start()
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn(managers, AIM, cid)
	var sp: Node = copies["Server"]
	var cp: Node = copies["ClientA"]
	cp.aim_source = Vector2(3, 4)
	cp.cursor = Vector2(7, 8)
	assert_true(await net.wait_until(func() -> bool: return sp.aim_seen == Vector2(3, 4)), "the server read the owner's custom value")
	assert_eq(cp.aim_seen, Vector2(3, 4), "the owner reads it in the same tick")
	assert_true(await net.wait_until(func() -> bool: return sp.cursor == Vector2(7, 8)), "the input property was written on the server")
	assert_gt(cp.net_input_calls, 0, "_net_input runs on the owner")
	assert_eq(sp.net_input_calls, 0, "not on the server for a client's entity")
	var inp: NetInput = Net.input(sp)
	assert_false(inp.is_live())
	inp.set("aim", Vector2(9, 9))
	await get_tree().physics_frame
	assert_eq(sp.aim_seen, Vector2(3, 4), "set() is ignored where the peer does not own the entity")
	assert_eq(inp.tick, net.server_session.tick(), "the server's input is for its tick")


func test_net_input_outside_a_session_reads_input() -> void:
	var p: Node = PLAYER.instantiate()
	add_child(p)
	var inp: NetInput = Net.input(p)
	assert_true(inp.is_live())
	Input.action_press("right")
	assert_true(inp.is_action_pressed("right"))
	assert_eq(inp.get_vector("left", "right", "up", "down"), Vector2(1, 0))
	inp.set("aim", Vector2(3, 4))
	assert_eq(inp.get("aim"), Vector2(3, 4), "custom values on a live input")
	assert_true(inp.actions().has("jump"))
	assert_false(inp.actions().has("ui_accept"))
	assert_true(Net.is_fresh())
