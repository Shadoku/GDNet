extends TestCase
## WP18 (docs/PLAN_NETCODE.md 12.5, "Idle entities"): resting things cost no polling. A sleeping
## `RigidBody2D` / `RigidBody3D` is off the hot list until it wakes; any other entity whose values did
## not change for `gdnet/sync/idle_after_ticks` is polled every `gdnet/sync/resting_poll_ticks` (24)
## until one changes, as are a sleeping body's cold properties; `Net.touch`, a call, an owner change and
## travel poll it at once; predicted players are always hot.
## The benchmarks print the polling time with `--verbose`. Headless over NetPipePeer.

const BODY := preload("res://tests/fixtures/idle/body.tscn")
const BODY_3D := preload("res://tests/fixtures/idle/body_3d.tscn")
const MARKER := preload("res://tests/fixtures/idle/marker.tscn")
const SNAP := preload("res://tests/fixtures/idle/snap_marker.tscn")
const PLAYER := preload("res://tests/fixtures/net/predicted_player.tscn")
const FALLING := preload("res://tests/fixtures/idle/falling_player.tscn")
const IDLE_AFTER := "gdnet/sync/idle_after_ticks"
const RESTING := "gdnet/sync/resting_poll_ticks"
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]

## The polling target of the plan (12.5): 5,000 resting entities under 0.5 ms per server tick.
const TARGET_MS := 0.5

var net: NetFixture
var _idle_after: Variant = null
var _resting: Variant = null


func before_each() -> void:
	_idle_after = ProjectSettings.get_setting(IDLE_AFTER, null)
	_resting = ProjectSettings.get_setting(RESTING, null)


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	ProjectSettings.set_setting(IDLE_AFTER, _idle_after)
	ProjectSettings.set_setting(RESTING, _resting)
	Net.resting_poll_ticks = 0
	Net.rejoin_timeout_s = -2.0
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _stat(s: NetSession, key: String) -> int:
	return int(s.stats().get(key, -1))


## Waits until the client's copy of every node in `names` exists under `World`.
func _copies(f: NetFixture, client: Node, names: Array) -> Array:
	var out := []
	for n: String in names:
		assert_true(await f.wait_until(func() -> bool: return client.has_node("World/" + n)), "the client has " + n)
		out.append(client.get_node_or_null("World/" + n))
	return out


## The physics frame (counted from now) at which each condition first held, checked after every
## physics frame for up to `limit` frames (-1: never).
func _first_frames(conds: Array, limit: int) -> Array[int]:
	var at: Array[int] = []
	at.resize(conds.size())
	at.fill(-1)
	for frame in limit:
		await get_tree().physics_frame
		var done := true
		for i in conds.size():
			if at[i] == -1 and (conds[i] as Callable).call():
				at[i] = frame
			done = done and at[i] != -1
		if done:
			break
	return at


## Adds a hot reference marker (its rotation changes every tick) and `count` still markers under the
## server's manager; returns [reference, stills, reference's copy, stills' copies].
func _reference_and_still(f: NetFixture, managers: Dictionary, count: int = 1) -> Array:
	var r: Node2D = SNAP.instantiate()
	r.name = "Ref"
	managers["Server"].add_child(r)
	var stills: Array[Node2D] = []
	var names := ["Ref"]
	for i in count:
		var x: Node2D = SNAP.instantiate()
		x.name = "Still%d" % i
		x.position = Vector2(500, 40 * i)
		managers["Server"].add_child(x)
		stills.append(x)
		names.append(String(x.name))
	f.server_session.ticked.connect(func(_t: int) -> void:
		if is_instance_valid(r):
			r.rotation = wrapf(r.rotation + 0.01, -PI, PI))
	var c := await _copies(f, f.clients[0], names)
	return [r, stills, c[0], c.slice(1)]


func test_a_sleeping_body_is_not_polled_and_a_woken_one_is_sent_in_the_tick_it_wakes() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		# The reference body never sleeps and the idle rule is off, so it is polled every tick as before.
		ProjectSettings.set_setting(IDLE_AFTER, 0)
		var managers := f.add_managers()
		var a: RigidBody2D = BODY.instantiate()
		a.name = "Awake"
		a.can_sleep = false
		var b: RigidBody2D = BODY.instantiate()
		b.name = "Sleeper"
		b.position = Vector2(200, 0)
		managers["Server"].add_child(a)
		managers["Server"].add_child(b)
		var copies := await _copies(f, f.clients[0], ["Awake", "Sleeper"])
		assert_true(await f.wait_until(func() -> bool: return b.sleeping and _stat(f.server_session, "sleeping_entities") == 1, 5.0), "the body falls asleep and is counted")
		assert_eq(_stat(f.server_session, "hot_entities"), 1, "only the awake body is hot")
		var polled := 0
		for i in 24:
			await get_tree().physics_frame
			polled += _stat(f.server_session, "polled_entities")
		assert_eq(polled, 24 + 1, "24 ticks: the awake body every tick, the sleeper's cold properties once (resting_poll_ticks)")
		# Both are pushed in the same tick; the woken one reaches the client no later than the awake one.
		var a0: float = copies[0].position.x
		var b0: float = copies[1].position.x
		a.apply_central_impulse(Vector2(120, 0))
		b.apply_central_impulse(Vector2(120, 0))
		var at := await _first_frames([
			func() -> bool: return copies[0].position.x > a0 + 0.05,
			func() -> bool: return copies[1].position.x > b0 + 0.05], 60)
		print("pushed: the awake body arrived after %d frames, the woken one after %d (mode %d)" % [at[0], at[1], f.host_mode])
		assert_ne(at[1], -1, "the woken body arrived")
		assert_true(at[1] <= at[0], "the woken body arrives no later than the awake one (%d vs %d)" % [at[1], at[0]])
		assert_false(b.sleeping)
		assert_eq(_stat(f.server_session, "sleeping_entities"), 0, "awake again"))
	assert_true(ok)


## Lets `count` still markers (consecutive net ids, so consecutive phases of the stagger) go idle beside
## a hot reference, moves them all by script in one tick, and returns how many ticks after the hot one
## each move was seen on the client, sorted.
func _idle_lateness(f: NetFixture, count: int) -> Array[int]:
	ProjectSettings.set_setting(IDLE_AFTER, 30)
	var m := await _reference_and_still(f, f.add_managers(), count)
	var r: Node2D = m[0]
	var stills: Array = m[1]
	await _frames(36)
	assert_eq(_stat(f.server_session, "idle_entities"), count, "the still markers went idle")
	assert_eq(_stat(f.server_session, "hot_entities"), 1, "the reference is hot")
	var r0: float = m[2].position.x
	var conds := [func() -> bool: return m[2].position.x > r0 + 50.0]
	r.position.x += 100.0
	for i in stills.size():
		var copy: Node2D = m[3][i]
		var x0 := copy.position.x
		conds.append(func() -> bool: return copy.position.x > x0 + 50.0)
		stills[i].position.x += 100.0
	var at := await _first_frames(conds, 90)
	var late: Array[int] = []
	for i in range(1, at.size()):
		assert_ne(at[i], -1, "idle marker %d's move arrived" % (i - 1))
		late.append(at[i] - at[0])
	late.sort()
	assert_true(await f.wait_until(func() -> bool: return _stat(f.server_session, "idle_entities") == 0), "each is hot after its move")
	return late


func test_an_unmoving_node_goes_idle_and_a_script_moving_it_is_seen_within_resting_poll_ticks() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		# As many markers as resting_poll_ticks: one in every phase of the resting stagger.
		var resting: int = f.server_session.stats()["resting_poll_ticks"]
		var cold: int = f.server_session.stats()["cold_poll_ticks"]
		assert_eq(resting, 24, "the default")
		var late := await _idle_lateness(f, resting)
		print("a script moving an idle node: seen %s ticks after a hot one (resting_poll_ticks %d; mode %d)" % [late, resting, f.host_mode])
		assert_true(late[0] >= 0, "never before the hot one")
		assert_true(late[-1] <= resting, "within resting_poll_ticks (%d > %d)" % [late[-1], resting])
		assert_gt(late[-1], cold, "polled at the resting rate, slower than cold_poll_ticks"))
	assert_true(ok)


func test_resting_poll_ticks_6_polls_an_idle_node_within_cold_poll_ticks() -> void:
	ProjectSettings.set_setting(RESTING, 6)
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var cold: int = f.server_session.stats()["cold_poll_ticks"]
		assert_eq(f.server_session.stats()["resting_poll_ticks"], 6)
		var late := await _idle_lateness(f, 6)
		print("resting_poll_ticks 6: seen %s ticks after a hot one (cold_poll_ticks %d; mode %d)" % [late, cold, f.host_mode])
		assert_true(late[0] >= 0, "never before the hot one")
		assert_true(late[-1] <= cold, "within cold_poll_ticks (%d > %d)" % [late[-1], cold])
		assert_gt(late[-1], 0, "an idle entity is not polled every tick"))
	assert_true(ok)


## The setting, `Net.resting_poll_ticks` and `NetSession.resting_poll_ticks` (the session's first, then
## `Net`'s, then the setting), read live; a value below `cold_poll_ticks` is raised to it.
func test_resting_poll_ticks_is_read_live_and_never_below_cold_poll_ticks() -> void:
	net = NetFixture.new(self, 0)
	net.start_server()
	var s := net.server_session
	assert_eq(s.stats()["resting_poll_ticks"], 24, "the default")
	assert_eq(Net.resting_poll_ticks, 24, "Net reads the effective value")
	ProjectSettings.set_setting(RESTING, 30)
	assert_eq(s.stats()["resting_poll_ticks"], 30, "the setting, live")
	ProjectSettings.set_setting(RESTING, 2)
	assert_eq(s.stats()["resting_poll_ticks"], 6, "raised to cold_poll_ticks")
	Net.resting_poll_ticks = 12
	assert_eq(Net.resting_poll_ticks, 12)
	assert_eq(s.stats()["resting_poll_ticks"], 12, "Net's over the setting")
	s.resting_poll_ticks = 40
	assert_eq(s.stats()["resting_poll_ticks"], 40, "the session's over Net's")
	s.cold_poll_ticks = 50
	assert_eq(s.stats()["resting_poll_ticks"], 50, "never below the session's cold_poll_ticks")
	s.cold_poll_ticks = 0
	s.resting_poll_ticks = 0
	assert_eq(s.stats()["resting_poll_ticks"], 12, "0: Net's again")
	# A live change re-buckets the resting entities: an idle node is still polled once a period.
	ProjectSettings.set_setting(IDLE_AFTER, 2)
	var managers := net.add_managers()
	var x: Node2D = MARKER.instantiate()
	managers["Server"].add_child(x)
	assert_true(await net.wait_until(func() -> bool: return _stat(s, "idle_entities") == 1))
	for period in [12, 7, 24]:
		Net.resting_poll_ticks = period
		await get_tree().physics_frame
		var polled := 0
		for i in period:
			await get_tree().physics_frame
			polled += _stat(s, "polled_entities")
		assert_eq(polled, 1, "polled once in %d ticks at resting_poll_ticks %d" % [period, period])


func test_touch_and_a_call_poll_an_idle_entity_in_the_same_tick() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		ProjectSettings.set_setting(IDLE_AFTER, 5)
		var m := await _reference_and_still(f, f.add_managers())
		var r: Node2D = m[0]
		var x: Node2D = m[1][0]
		for round in 4:
			await _frames(9 + round)
			assert_eq(_stat(f.server_session, "idle_entities"), 1, "idle before round %d" % round)
			var r0: float = m[2].position.x
			var x0: float = m[3][0].position.x
			r.position.x += 100.0
			if round % 2 == 0:
				x.position.x += 100.0
				Net.touch(x.get_node("NetEntity") if round == 2 else x)
			else:
				Net.server(x.teleport, Vector2(x.position.x + 100.0, 0.0))
			var at := await _first_frames([
				func() -> bool: return m[2].position.x > r0 + 50.0,
				func() -> bool: return m[3][0].position.x > x0 + 50.0], 60)
			assert_eq(at[1], at[0], "round %d (%s): arrives with the hot one" % [round, "call" if round % 2 else "touch"])
		# NetSession.touch does the same as Net.touch.
		await _frames(10)
		assert_eq(_stat(f.server_session, "idle_entities"), 1)
		f.server_session.touch(x)
		await get_tree().physics_frame
		assert_eq(_stat(f.server_session, "idle_entities"), 0, "touched: hot"))
	assert_true(ok)


func test_a_cold_property_changed_on_a_sleeping_body_arrives() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var managers := f.add_managers()
		var b: RigidBody2D = BODY.instantiate()
		b.name = "Sleeper"
		managers["Server"].add_child(b)
		var c: Array = await _copies(f, f.clients[0], ["Sleeper"])
		assert_true(await f.wait_until(func() -> bool: return _stat(f.server_session, "sleeping_entities") == 1, 5.0))
		var resting: int = f.server_session.stats()["resting_poll_ticks"]
		b.modulate = Color(1, 0, 0)
		var at := await _first_frames([func() -> bool: return c[0].modulate.is_equal_approx(Color(1, 0, 0))], 60)
		print("a cold property of a sleeping body arrived after %d frames (resting_poll_ticks %d; mode %d)" % [at[0], resting, f.host_mode])
		assert_ne(at[0], -1, "it arrived")
		assert_true(at[0] <= resting + 3, "within resting_poll_ticks plus the trip")
		assert_true(b.sleeping, "the body still sleeps")
		assert_true(await f.wait_until(func() -> bool: return _stat(f.server_session, "sleeping_entities") == 1, 2.0), "and is off the hot list again")
		# Destroyed while asleep: its copy goes, and nothing is left counted (or connected).
		b.queue_free()
		assert_true(await f.wait_until(func() -> bool: return not f.clients[0].has_node("World/Sleeper")), "the copy is destroyed")
		assert_eq(_stat(f.server_session, "sleeping_entities"), 0)
		assert_eq(_stat(f.server_session, "hot_entities"), 0))
	assert_true(ok)


## A sleeping body that travels to another level is polled in the tick it arrives, sleeps again under
## its new manager, and that manager hears it wake.
func test_a_sleeping_body_travels_and_stays_watched() -> void:
	var spacing: Variant = ProjectSettings.get_setting("gdnet/levels/spacing", null)
	ProjectSettings.set_setting("gdnet/levels/spacing", null)
	net = NetFixture.new(self, 0)
	net.start_server()
	var a: Node = net.server_session.load_level("res://tests/fixtures/rooms/chunk_a.tscn")
	net.server_session.load_level("res://tests/fixtures/rooms/chunk_b.tscn")
	var b: RigidBody2D = BODY.instantiate()
	b.name = "Crate"
	a.get_node("NetManager").add_child(b)
	assert_true(await net.wait_until(func() -> bool: return b.sleeping and _stat(net.server_session, "sleeping_entities") == 1, 3.0), "asleep in A")
	net.server_session.travel(b, "res://tests/fixtures/rooms/chunk_b.tscn")
	assert_true(await net.wait_until(func() -> bool: return String(b.get_parent().get_parent().scene_file_path).ends_with("chunk_b.tscn")), "it moved to B")
	assert_true(b.sleeping)
	assert_true(await net.wait_until(func() -> bool: return _stat(net.server_session, "sleeping_entities") == 1, 3.0), "asleep in B")
	b.apply_central_impulse(Vector2(50, 0))
	assert_true(await net.wait_until(func() -> bool: return _stat(net.server_session, "hot_entities") == 1), "B's manager hears it wake")
	ProjectSettings.set_setting("gdnet/levels/spacing", spacing)


func test_idle_after_ticks_zero_turns_the_idle_rule_off() -> void:
	net = NetFixture.new(self, 1)
	net.start()
	assert_true(await net.wait_connected())
	ProjectSettings.set_setting(IDLE_AFTER, 0)
	var managers := net.add_managers()
	var x: Node2D = MARKER.instantiate()
	x.name = "Still"
	managers["Server"].add_child(x)
	await _copies(net, net.clients[0], ["Still"])
	await _frames(60)
	assert_eq(_stat(net.server_session, "idle_entities"), 0, "never idle")
	assert_eq(_stat(net.server_session, "hot_entities"), 1)
	assert_eq(_stat(net.server_session, "polled_entities"), 1, "polled every tick")
	# Turning the rule on again lets it go idle; off again wakes it at once.
	ProjectSettings.set_setting(IDLE_AFTER, 5)
	await _frames(10)
	assert_eq(_stat(net.server_session, "idle_entities"), 1, "idle with the rule on")
	ProjectSettings.set_setting(IDLE_AFTER, 0)
	await _frames(2)
	assert_eq(_stat(net.server_session, "idle_entities"), 0, "hot again once it is off")


## A level's body that fell asleep before the session started is adopted asleep: the server's create
## still carries its state (the client's own copy of the level sits elsewhere), and it is off the hot
## list from its second poll. A body spawned with `sleeping = true` (Godot wakes it as it enters the
## tree) arrives with its state too.
func test_a_body_asleep_when_adopted_or_spawned_arrives_with_its_state() -> void:
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		net = NetFixture.new(self, 1, mode)
		var managers := net.add_managers()
		var b: RigidBody2D = BODY.instantiate()
		b.name = "Level"
		b.position = Vector2(321, 45)
		b.rotation = 0.5
		b.modulate = Color(0, 1, 0)
		managers["Server"].add_child(b)
		var mine: RigidBody2D = BODY.instantiate()
		mine.name = "Level"
		managers["ClientA"].add_child(mine)
		assert_true(await net.wait_until(func() -> bool: return b.sleeping, 3.0), "it sleeps before the session")
		net.start()
		assert_true(await net.wait_connected())
		assert_true(await net.wait_until(func() -> bool:
			return mine.position.distance_to(Vector2(321, 45)) < 0.1 and absf(mine.rotation - 0.5) < 0.01 and mine.modulate.is_equal_approx(Color(0, 1, 0)), 3.0),
			"the adopted copy has the server's state (%s, %f, %s; mode %d)" % [mine.position, mine.rotation, mine.modulate, mode])
		assert_true(b.sleeping, "still asleep")
		assert_eq(_stat(net.server_session, "sleeping_entities"), 1, "counted as sleeping")
		assert_eq(_stat(net.server_session, "hot_entities"), 0)
		var s: RigidBody2D = BODY.instantiate()
		s.name = "Born"
		s.sleeping = true
		s.position = Vector2(-50, 70)
		s.modulate = Color(0, 0, 1)
		managers["Server"].add_child(s)
		var c: Array = await _copies(net, net.clients[0], ["Born"])
		var copy: RigidBody2D = c[0]
		assert_true(await net.wait_until(func() -> bool:
			return copy.position.distance_to(Vector2(-50, 70)) < 0.1 and copy.modulate.is_equal_approx(Color(0, 0, 1)), 3.0),
			"the spawned copy has the body's state")
		assert_true(await net.wait_until(func() -> bool: return _stat(net.server_session, "sleeping_entities") == 2, 3.0), "both sleep")
		net.teardown()
		net = null


func test_a_predicted_player_standing_still_never_goes_idle() -> void:
	ProjectSettings.set_setting(IDLE_AFTER, 5)
	net = NetFixture.new(self, 1, NetFixture.HostMode.DEDICATED)
	var link := NetEmulation.new()
	link.send_latency_ms = 50.0
	link.receive_latency_ms = 50.0
	net.set_emulation(net.clients[0], link)
	net.start()
	assert_true(await net.wait_connected())
	var managers := net.add_managers()
	var cid := net.peer_id(net.clients[0])
	var sp: Node2D = Net.spawn_for(cid, PLAYER, managers["Server"], "P")
	var c: Array = await _copies(net, net.clients[0], ["P"])
	var cp: Node2D = c[0]
	var rolled := []
	cp.get_node("NetEntity").rolled_back.connect(func(a: int, b: int) -> void: rolled.append([a, b]))
	await _frames(30)
	assert_eq(_stat(net.server_session, "idle_entities"), 0, "a predicted player is never idle")
	assert_eq(_stat(net.server_session, "hot_entities"), 1)
	var start := cp.position
	assert_eq(sp.position, start)
	Input.action_press("right")
	await get_tree().physics_frame
	assert_gt(cp.position.x, start.x, "the owner moves on the tick after the press")
	await _frames(30)
	assert_gt(sp.position.x, start.x, "the server follows")
	Input.action_release("right")
	assert_true(await net.wait_until(func() -> bool: return sp.position.distance_to(cp.position) < 0.1, 3.0), "they converge")
	assert_eq(rolled, [], "no rollback")


func test_an_away_seats_falling_body_keeps_being_sent() -> void:
	ProjectSettings.set_setting(IDLE_AFTER, 5)
	net = NetFixture.new(self, 2, NetFixture.HostMode.DEDICATED)
	net.start()
	assert_true(await net.wait_connected())
	net.server_session.rejoin_timeout_s = 30.0
	var managers := net.add_managers()
	var aid := net.peer_id(net.clients[0])
	var sp: Node2D = Net.spawn_for(aid, FALLING, managers["Server"], "P")
	var b: Node = net.clients[1]
	var c: Array = await _copies(net, b, ["P"])
	await _copies(net, net.clients[0], ["P"])
	net.drop_client(0)
	assert_true(await net.wait_until(func() -> bool: return net.server_session.stats()["seats_away"] == 1), "the seat is away")
	await _frames(10)
	var y0: float = (c[0] as Node2D).position.y
	var s0 := sp.position.y
	await _frames(30)
	assert_gt(sp.position.y, s0 + 10.0, "the server keeps simulating the away seat's body")
	assert_gt((c[0] as Node2D).position.y, y0 + 10.0, "and the other client keeps seeing it fall")
	assert_eq(_stat(net.server_session, "idle_entities"), 0)


## Spawns `count` copies of `scene` under the server's manager of a server-only fixture, `sleeping` for
## bodies, in a grid; returns them once every one is spawned.
func _spawn_many(scene: PackedScene, count: int, asleep: bool) -> Array[Node]:
	net = NetFixture.new(self, 0)
	net.start_server()
	var managers := net.add_managers()
	var nodes: Array[Node] = []
	for i in count:
		var n: Node = scene.instantiate()
		n.name = "B%d" % i
		if n is Node2D:
			(n as Node2D).position = Vector2(i % 100, i / 100) * 10.0
		elif n is Node3D:
			(n as Node3D).position = Vector3(i % 100, 0, i / 100)
		if asleep and (n is RigidBody2D or n is RigidBody3D):
			n.sleeping = true
		managers["Server"].add_child(n)
		nodes.append(n)
	assert_true(await net.wait_until(func() -> bool: return net.server_session.entity_count() == count, 30.0))
	return nodes


## The average and the worst `poll_ms` over `ticks` server ticks, running `each_tick` before every one.
func _poll_ms(ticks: int, each_tick: Callable = Callable()) -> Vector2:
	var samples: Array[float] = []
	var sampler := func(_t: int) -> void:
		samples.append(float(net.server_session.stats()["poll_ms"]))
	net.server_session.ticked.connect(sampler)
	for i in ticks + 1:
		if each_tick.is_valid():
			each_tick.call()
		await get_tree().physics_frame
	net.server_session.ticked.disconnect(sampler)
	samples.pop_front()
	var total := 0.0
	var worst := 0.0
	for s in samples:
		total += s
		worst = maxf(worst, s)
	return Vector2(total / samples.size(), worst)


## The limit. The plan's target (12.5) is 0.5 ms per tick for 5,000 resting entities. Measured on this
## machine (4 cores, release build, `resting_poll_ticks` 24), alone: 0.32-0.42 ms for sleeping
## `RigidBody2D`s, 0.28-0.32 ms for `RigidBody3D`s, 0.20-0.27 ms for idle `Node2D`s; in the integration
## stage's four processes 0.31-0.42 ms. Nearly all of it is the resting polls themselves (a 24th of the
## entities each tick, about 1.5 microseconds apiece: the sleep and freeze flags and 3-6 property reads),
## so the cost follows `resting_poll_ticks` (printed at 6, the previous rate: 1.1-1.9 ms). The assertion
## is 1.0 ms, more than twice the loaded figure and below what the previous rate costs, so it stays green
## under load and fails if resting entities fall back to the cold rate; the figure itself is printed.
const LIMIT_MS := 1.0


## The average resting `poll_ms` at the current settings, over two resting periods, after one period in
## which each entity's first resting poll records its values once.
func _rest_ms() -> Vector2:
	var period: int = net.server_session.stats()["resting_poll_ticks"]
	await _frames(period + 1)
	return await _poll_ms(2 * period)


func _bench_bodies(scene: PackedScene, label: String) -> void:
	timeout_sec = 60.0
	# Entering the tree wakes a body; with nothing moving it, Godot puts it to sleep half a second later.
	var bodies := await _spawn_many(scene, 5000, true)
	await _frames(20)
	assert_true(await net.wait_until(func() -> bool: return _stat(net.server_session, "sleeping_entities") == 5000, 5.0), "every body sleeps")
	var rest := await _rest_ms()
	assert_eq(_stat(net.server_session, "sleeping_entities"), 5000, "still asleep")
	# The previous rate, for comparison: every cold_poll_ticks.
	net.server_session.resting_poll_ticks = 6
	var rest6 := await _rest_ms()
	net.server_session.resting_poll_ticks = 0
	for b in bodies:
		if b is RigidBody2D:
			(b as RigidBody2D).linear_velocity = Vector2(10, 0)
		else:
			(b as RigidBody3D).linear_velocity = Vector3(1, 0, 0)
	await _frames(3)
	assert_eq(_stat(net.server_session, "hot_entities"), 5000, "every body woke")
	var moving := await _poll_ms(12)
	print("BENCHMARK: polling 5000 %s: resting %.3f ms per tick (worst %.3f; %.3f at resting_poll_ticks 6), moving %.3f ms (target %.1f ms)" % [label, rest.x, rest.y, rest6.x, moving.x, TARGET_MS])
	assert_lt(rest.x, LIMIT_MS, "5,000 resting bodies poll in under %.2f ms per tick" % LIMIT_MS)
	assert_lt(rest.x, rest6.x, "the resting rate costs less than polling them every cold_poll_ticks")


func test_benchmark_5000_resting_rigid_bodies_2d() -> void:
	await _bench_bodies(BODY, "RigidBody2D")


func test_benchmark_5000_resting_rigid_bodies_3d() -> void:
	await _bench_bodies(BODY_3D, "RigidBody3D")


func test_benchmark_5000_idle_nodes() -> void:
	timeout_sec = 60.0
	var nodes := await _spawn_many(MARKER, 5000, false)
	await _frames(32)
	assert_eq(_stat(net.server_session, "idle_entities"), 5000, "every node is idle")
	var rest := await _rest_ms()
	net.server_session.resting_poll_ticks = 6
	var rest6 := await _rest_ms()
	net.server_session.resting_poll_ticks = 0
	assert_eq(_stat(net.server_session, "idle_entities"), 5000, "still idle")
	var moving := await _poll_ms(12, func() -> void:
		for n in nodes:
			(n as Node2D).position.x += 1.0)
	print("BENCHMARK: polling 5000 Node2D: idle %.3f ms per tick (worst %.3f; %.3f at resting_poll_ticks 6), moving %.3f ms (target %.1f ms)" % [rest.x, rest.y, rest6.x, moving.x, TARGET_MS])
	assert_lt(rest.x, LIMIT_MS, "5,000 idle nodes poll in under %.2f ms per tick" % LIMIT_MS)
	assert_lt(rest.x, rest6.x, "the resting rate costs less than polling them every cold_poll_ticks")
