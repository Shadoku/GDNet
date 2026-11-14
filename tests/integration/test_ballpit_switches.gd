extends TestCase
## WP19 (docs/PLAN_NETCODE.md 12.4, 12.6): the Ballpit demo's two switches on a running pit. The HUD's
## physics selector (host) sets who simulates the balls on every peer, and fits the paddles' masks; its
## prediction selector (each client, for its own screen) chooses how the frozen balls are shown. Apart
## from `test_ballpit_demo.gd` only to balance the integration stage's files.

const PIT := preload("res://demos/ballpit/pit.tscn")
const HUD := preload("res://demos/ballpit/hud.tscn")
## A replica holds the server's position to half a 0.1 px step per axis (D17): 0.0707 px.
const CONVERGED_PX := 0.075

var net: NetFixture


func after_each() -> void:
	Input.action_release("left")
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
	e.seed = 5
	return e


## A pit in every branch, the clients' on collision layers of their own, every pit started.
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


## A HUD for `branch`'s pit, its selectors wired but not started (no meter in a test).
func _hud(branch: Node) -> BallpitHud:
	var hud: BallpitHud = HUD.instantiate()
	hud.pit = branch.get_node("Pit")
	branch.add_child(hud)
	return hud


## Picks `text` in one of the HUD's selectors, as a click does.
func _choose(hud: BallpitHud, selector: String, text: String) -> void:
	var o: OptionButton = hud.get_node(selector)
	for i in o.item_count:
		if o.get_item_text(i) == text:
			o.select(i)
			o.item_selected.emit(i)
			return
	fail("%s has no item %s" % [selector, text])


func _balls(branch: Node) -> Array[RigidBody2D]:
	var out: Array[RigidBody2D] = []
	for n in branch.get_node("Pit/NetManager").get_children():
		if n is RigidBody2D and not n.is_queued_for_deletion():
			out.append(n)
	return out


func _copy(c: Node, b: Node) -> RigidBody2D:
	return c.get_node("Pit/NetManager/" + String(b.name)) as RigidBody2D


func _paddle(branch: Node) -> CharacterBody2D:
	for n in branch.get_node("Pit/NetManager").get_children():
		if n is CharacterBody2D:
			return n
	return null


## A server pile asleep for half a second.
func _settle(f: NetFixture, timeout: float) -> bool:
	var still := [0]
	return await f.wait_until(func() -> bool:
		still[0] = still[0] + 1 if _balls(f.server).all(func(b: RigidBody2D) -> bool: return b.sleeping) else 0
		return still[0] >= Engine.physics_ticks_per_second / 2, timeout)


## The largest distance between a server ball and the client's copy (rotation as 10 px/rad).
func _max_error(server: Node, c: Node) -> float:
	var worst := 0.0
	for b in _balls(server):
		var cb := _copy(c, b)
		worst = maxf(worst, b.position.distance_to(cb.position))
		worst = maxf(worst, 10.0 * absf(angle_difference(b.rotation, cb.rotation)))
	return worst


## Every ball on `branch` holds `mode`.
func _all_in(branch: Node, mode: String) -> bool:
	return _balls(branch).all(func(b: RigidBody2D) -> bool: return Net.physics_of(b) == mode)


func test_the_physics_switch_reaches_every_peer_and_back() -> void:
	timeout_sec = 50.0
	var ok := await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var c: Node = f.clients[0]
		_setup(f, 60)
		var hud := _hud(f.server)
		var pit: BallpitSpawner = f.server.get_node("Pit")
		assert_true(await f.wait_until(func() -> bool: return f.session(c).entity_count() == 60 + f.players().size(), 5.0), "every ball and paddle reached the client")
		assert_true(await _settle(f, 8.0), "settled")
		assert_true(await f.wait_until(func() -> bool: return _max_error(f.server, c) < CONVERGED_PX, 3.0), "converged")
		var paddle := _paddle(c)
		var server_paddle := _paddle(f.server)
		assert_eq(paddle.collision_mask, 1 << 3, "server balls: the client's paddle collides with the walls only")
		# simulated: every peer runs the balls; the client's are live and corrected by the snapshots.
		_choose(hud, "Physics", "simulated")
		assert_true(await f.wait_until(func() -> bool: return _all_in(c, "simulated"), 2.0), "simulated reached the client")
		assert_true(_all_in(f.server, "simulated"), "and the server")
		assert_true(_balls(c).all(func(b: RigidBody2D) -> bool: return not b.freeze), "the client's balls are live")
		assert_eq(paddle.collision_mask, 3 << 3, "the client's paddle collides with live balls")
		assert_eq(server_paddle.collision_mask, 3, "so does the server's")
		var sim0: int = f.session(c).stats()["sim_corrections"]
		pit.shake()
		assert_true(await _settle(f, 10.0), "simulated: the shaken pile settles on the server")
		await _frames(Engine.physics_ticks_per_second)
		var sim_error := _max_error(f.server, c)
		var sims: int = f.session(c).stats()["sim_corrections"] - sim0
		var sim_awake := _balls(c).filter(func(b: RigidBody2D) -> bool: return not b.sleeping).size()
		# A second after the server's pile fell asleep, the client's has settled onto it: every resting
		# replica put at its server ball's pose (the quantiser's half step) and asleep.
		assert_lt(sim_error, CONVERGED_PX, "simulated: the client's pile settled onto the server's: %.4f px" % sim_error)
		assert_eq(sim_awake, 0, "simulated: every client ball sleeps")
		# handoff: nobody touches the balls, so the server holds them all, frozen elsewhere.
		_choose(hud, "Physics", "handoff")
		assert_true(await f.wait_until(func() -> bool: return _all_in(c, "handoff"), 2.0), "handoff reached the client")
		assert_true(_balls(c).all(func(b: RigidBody2D) -> bool: return b.freeze), "the server holds every ball: frozen on the client")
		assert_true(await f.wait_until(func() -> bool: return _max_error(f.server, c) < CONVERGED_PX, 4.0), "handoff: the client converged on the server's pile: %.4f px" % _max_error(f.server, c))
		assert_eq(int(f.server_session.stats()["handoff_claims"]), 0, "nobody touched a ball: no claims")
		# server: back to v1's behaviour.
		_choose(hud, "Physics", "server")
		assert_true(await f.wait_until(func() -> bool: return _all_in(c, "server"), 2.0), "server reached the client")
		assert_eq(paddle.collision_mask, 1 << 3, "the paddle's mask is the walls again")
		pit.shake()
		assert_true(await _settle(f, 10.0), "server: the shaken pile settles")
		assert_true(await f.wait_until(func() -> bool: return _max_error(f.server, c) < CONVERGED_PX, 4.0), "server: the client converged: %.4f px" % _max_error(f.server, c))
		print("ballpit physics switch (mode %d): simulated, a second after the shaken pile slept on the server: %.2f px apart, %d corrections; handoff and server converged to %.3f px" % [f.host_mode, sim_error, sims, _max_error(f.server, c)])
	)
	assert_true(ok, "fixtures connected")


func test_a_paddle_push_in_handoff_moves_a_ball_at_once() -> void:
	timeout_sec = 30.0
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	_setup(net, 10)
	var hud := _hud(net.server)
	assert_true(await net.wait_until(func() -> bool: return net.session(c).entity_count() == 11, 5.0))
	_choose(hud, "Physics", "handoff")
	assert_true(await net.wait_until(func() -> bool: return _all_in(c, "handoff"), 2.0), "handoff everywhere")
	# The paddle goes to the floor, right of the row of balls (x 12 to 196), and waits there.
	var sp := _paddle(net.server)
	var cp := _paddle(c)
	sp.position = Vector2(320, 390)
	assert_true(await net.wait_until(func() -> bool: return cp.position.distance_to(sp.position) < 0.5, 3.0), "the owner's paddle is at the floor")
	assert_true(await _settle(net, 8.0), "settled")
	await _frames(20)
	var start := {}
	for b in _balls(net.server):
		start[b.name] = [b.position, _copy(c, b).position]
	var r0: int = net.session(c).stats()["rollbacks"]
	var contact := -1
	var pusher := -1
	var server := -1
	Input.action_press("left")
	for t in 50:
		await get_tree().physics_frame
		if contact < 0:
			for i in cp.get_slide_collision_count():
				if cp.get_slide_collision(i).get_collider() is RigidBody2D:
					contact = t
		for b in _balls(net.server):
			if pusher < 0 and _copy(c, b).position.distance_to(start[b.name][1]) > 0.2:
				pusher = t
			if server < 0 and b.position.distance_to(start[b.name][0]) > 0.2:
				server = t
	Input.action_release("left")
	var claims: int = net.session(c).stats()["handoff_claims"]
	var rollbacks: int = net.session(c).stats()["rollbacks"] - r0
	assert_gt(contact, -1, "the paddle touched a ball (handoff balls are in its mask)")
	assert_gt(claims, 0, "the pusher claimed what it pushed")
	assert_true(pusher >= 0 and pusher <= contact + 1, "the pusher sees the ball move on the step after the contact: contact %d, moved %d" % [contact, pusher])
	assert_gt(server, pusher, "the server sees it later")
	assert_true(await _settle(net, 10.0), "the pushed balls settle")
	assert_true(await net.wait_until(func() -> bool: return int(net.session(c).stats()["handoff_held"]) == 0, 4.0), "the pusher gave every ball back")
	assert_true(await net.wait_until(func() -> bool: return _max_error(net.server, c) < CONVERGED_PX, 4.0), "converged: %.4f px" % _max_error(net.server, c))
	print("ballpit handoff push (50 ms each way): contact at tick %d, the pusher saw a ball move at tick %d, the server at tick %d; %d claims, %d returns, %d rollbacks of the paddle" % [contact, pusher, server, claims, net.server_session.stats()["handoff_returns"], rollbacks])


## The server's ball against the client's copy while it flies: the mean distance from the 10th tick
## after the throw (the client has heard of it by then) to the 34th (before it lands).
func _flight_error(f: NetFixture, c: Node) -> float:
	(f.server.get_node("Pit") as BallpitSpawner).shake()
	var b: RigidBody2D = _balls(f.server)[0]
	var sum := 0.0
	for t in 34:
		await get_tree().physics_frame
		if t >= 10:
			sum += b.position.distance_to(_copy(c, b).position)
	return sum / 24.0


func test_dead_reckoning_shows_a_thrown_ball_nearer_the_servers_present() -> void:
	timeout_sec = 20.0
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	var c: Node = net.clients[0]
	net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected())
	_setup(net, 1)
	var hud := _hud(c)
	assert_true(await net.wait_until(func() -> bool: return net.session(c).entity_count() == 2, 5.0))
	assert_true(await _settle(net, 8.0), "settled")
	var ball: RigidBody2D = _balls(c)[0]
	assert_eq(Net.body_prediction_of(ball), NetEntity.BODY_PREDICTION_INTERPOLATE, "interpolated by default")
	var interpolated := await _flight_error(net, c)
	assert_true(await _settle(net, 8.0), "landed")
	_choose(hud, "Prediction", "dead_reckoning")
	assert_eq(Net.body_prediction_of(ball), NetEntity.BODY_PREDICTION_DEAD_RECKONING, "the client's own switch")
	assert_eq(Net.body_prediction_of(_balls(net.server)[0]), NetEntity.BODY_PREDICTION_NONE, "the server shows its own ball")
	var reckoned := await _flight_error(net, c)
	print("ballpit body prediction (50 ms each way): a thrown ball shown %.1f px from the server's present interpolated, %.1f px dead-reckoned" % [interpolated, reckoned])
	assert_lt(reckoned, interpolated / 4.0, "dead reckoning shows the ball nearer the present")
