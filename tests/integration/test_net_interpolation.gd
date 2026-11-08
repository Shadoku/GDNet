extends TestCase
## WP5: interpolation and smoothing (docs/PLAN_NETCODE.md 2.1 "Profiles" `interpolate`, 2.2 step 7, 2.5,
## gate G5's render half). Replicas show interpolated hot properties on a render timeline one adaptive
## jitter buffer behind the newest snapshot, with decaying error offsets and a teleport threshold; an
## owner blends a rollback's correction in `_process` while its `_physics_process` keeps exact values.
## Headless over NetPipePeer; positions are sampled in the replicas' own `_process`.

const MOVER := preload("res://tests/fixtures/net/mover.tscn")
const MOVER_SNAP := preload("res://tests/fixtures/net/mover_snap.tscn")
const MOVER_TELEPORT := preload("res://tests/fixtures/net/mover_teleport.tscn")
const BLEND := preload("res://tests/fixtures/net/blend_player.tscn")
const ACTIONS: Array[String] = ["left", "right", "up", "down", "jump"]
const SETTINGS: Array[String] = [
	"gdnet/interpolation/jitter_buffer_ticks",
	"gdnet/interpolation/adaptive",
	"gdnet/interpolation/teleport_distance",
]

var net: NetFixture
var _rate: int = 0
var _fps: int = 0


func before_each() -> void:
	_rate = Engine.physics_ticks_per_second
	_fps = Engine.max_fps


func after_each() -> void:
	for a in ACTIONS:
		Input.action_release(a)
	if net != null:
		net.teardown()
		net = null
	Engine.physics_ticks_per_second = _rate
	Engine.max_fps = _fps
	for s in SETTINGS:
		if ProjectSettings.has_setting(s):
			ProjectSettings.set_setting(s, null)


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


func _link(ms: float, variance: float = 0.0) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.receive_variance_ms = variance
	e.seed = 11
	return e


func _start(clients: int = 1, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED, emulation: NetEmulation = null) -> Dictionary:
	net = NetFixture.new(self, clients, mode)
	if emulation != null:
		for c in net.clients:
			net.set_emulation(c, emulation)
	net.start()
	assert_true(await net.wait_connected())
	return net.add_managers()


## Adds `scene` under the server's manager (owned by the server) and returns the copies by branch name.
func _spawn(managers: Dictionary, scene: PackedScene, node_name: String) -> Dictionary:
	var n: Node = scene.instantiate()
	n.name = node_name
	managers["Server"].add_child(n)
	var out := {"Server": n}
	for c in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path)), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	await _frames(10)
	return out


## Spawns `scene` for client peer `peer`, waits for the copies and a second of pings (the WP4 way).
func _spawn_for(managers: Dictionary, scene: PackedScene, peer: int, node_name: String) -> Dictionary:
	var sp: Node = Net.spawn_for(peer, scene, managers["Server"], node_name)
	var out := {"Server": sp}
	for c in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path)), "the copy reached " + String(c.name))
		out[String(c.name)] = c.get_node(path)
	await _frames(Engine.physics_ticks_per_second)
	return out


func _mean(a: Array) -> float:
	var s := 0.0
	for x: float in a:
		s += x
	return s / maxf(1.0, float(a.size()))


func _variance(a: Array, m: float) -> float:
	var s := 0.0
	for x: float in a:
		s += (x - m) * (x - m)
	return s / maxf(1.0, float(a.size()))


func test_constant_speed_renders_smoothly_at_60_and_144_hz() -> void:
	# Gate G5, render half: physics at 60 Hz, rendering at 60 and 144 Hz.
	timeout_sec = 40.0
	for fps: int in [60, 144]:
		Engine.max_fps = fps
		var managers := await _start()
		var copies := await _spawn(managers, MOVER, "M")
		var sm: Node2D = copies["Server"]
		var cm: Node2D = copies["ClientA"]
		sm.speed = Vector2(120.0, 0.0)
		var step: float = sm.speed.x / Engine.physics_ticks_per_second
		await wait_seconds(1.0)
		cm.other = sm
		cm.recording = true
		var f0 := Engine.get_process_frames()
		var t0 := Time.get_ticks_usec()
		await wait_seconds(2.0)
		cm.recording = false
		var seconds := float(Time.get_ticks_usec() - t0) / 1e6
		var achieved := float(Engine.get_process_frames() - f0) / seconds
		var steps: Array = []
		var backwards := 0
		for i in range(1, cm.shown.size()):
			var d: float = cm.shown[i].x - cm.shown[i - 1].x
			steps.append(d)
			if d < -0.001:
				backwards += 1
		var lags: Array = []
		for i in cm.shown.size():
			lags.append((cm.other_shown[i].x - cm.shown[i].x) / step)
		var m := _mean(steps)
		var v := _variance(steps, m)
		var stats: Dictionary = net.client_sessions[0].stats()
		print("render %d Hz (achieved %.1f): per-frame displacement mean %.3f px, variance %.4f px^2 (sd %.3f); lag behind the server %.2f ticks; render_lag_ticks %.2f; jitter buffer %d; late/s %d" % [fps, achieved, m, v, sqrt(v), _mean(lags), stats["render_lag_ticks"], stats["jitter_buffer_ticks"], stats["late_snapshots_per_s"]])
		assert_gt(cm.shown.size(), 30, "frames were recorded")
		assert_eq(backwards, 0, "the replica never moves backwards at %d Hz" % fps)
		var buffer: int = stats["jitter_buffer_ticks"]
		assert_almost_eq(stats["render_lag_ticks"], float(buffer), 1.0, "the render tick sits one jitter buffer behind the newest snapshot")
		assert_gt(_mean(lags), float(buffer) - 0.5, "the replica is at least one jitter buffer behind the server")
		assert_lt(_mean(lags), float(buffer) + 3.0, "and not much more (one tick of transit)")
		if achieved < fps * 0.85:
			print("the headless runner reached %.1f of %d frames per second: the variance bound is skipped" % [achieved, fps])
			skip("render rate %d not reached (%.1f)" % [fps, achieved])
		else:
			# Smooth: the per-frame displacement's standard deviation stays under a quarter of its mean (a
			# snapping replica at 144 Hz moves 0 px on most frames and 2 px on some: sd about 0.95 px for a
			# mean of 0.83, more than its mean).
			assert_lt(sqrt(v), 0.25 * m, "per-frame displacement is even at %d Hz" % fps)
		net.teardown()
		net = null
		await _frames(2)


func test_a_small_correction_blends_and_a_teleport_snaps() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var copies := await _spawn(managers, MOVER, "T")
	var sm: Node2D = copies["Server"]
	var cm: Node2D = copies["ClientA"]
	await _frames(20)
	assert_eq(cm.position, Vector2.ZERO)
	cm.recording = true
	sm.position.x += 20.0
	await wait_seconds(3.0)
	cm.recording = false
	var between := 0
	var biggest := 0.0
	var prev := 0.0
	for p: Vector2 in cm.shown:
		assert_lt(p.x, 20.001, "never overshoots")
		assert_gt(p.x, prev - 0.0001, "approaches monotonically")
		biggest = maxf(biggest, p.x - prev)
		prev = p.x
		if p.x > 0.5 and p.x < 19.5:
			between += 1
	print("20 px correction: %d frames on the way, largest step %.2f px, residual after 3 s %.4f px" % [between, biggest, 20.0 - cm.position.x])
	assert_gt(between, 10, "spread over several frames")
	assert_lt(biggest, 5.0, "no visible jump")
	assert_almost_eq(cm.position.x, 20.0, 0.01, "arrived")
	# Above the teleport threshold (200 px for a Vector2): one step.
	cm.clear_recording()
	cm.recording = true
	sm.position.x += 500.0
	await wait_seconds(1.0)
	cm.recording = false
	var midway := 0
	for p: Vector2 in cm.shown:
		if p.x > 21.0 and p.x < 519.0:
			midway += 1
	assert_eq(midway, 0, "a teleport snaps")
	assert_almost_eq(cm.position.x, 520.0, 0.01)
	assert_gt(net.client_sessions[0].stats()["offset_corrections_per_s"] + net.client_sessions[0].stats()["offset_corrections"], 0)


func test_a_rolled_back_owner_blends_in_process_and_simulates_exact_values() -> void:
	timeout_sec = 20.0
	var managers := await _start(1, NetFixture.HostMode.DEDICATED, _link(50.0))
	var cid := net.peer_id(net.clients[0])
	var copies := await _spawn_for(managers, BLEND, cid, "B")
	var sp: Node2D = copies["Server"]
	var cp: Node2D = copies["ClientA"]
	var rolled: Array = []
	cp.get_node("NetEntity").rolled_back.connect(func(f: int, t: int) -> void: rolled.append([f, t]))
	var before := cp.position
	assert_eq(sp.position, before)
	cp.record_frames = true
	sp.position.x += 50.0
	var target := sp.position
	assert_true(await net.wait_until(func() -> bool: return rolled.size() > 0), "the owner rolled back")
	await wait_seconds(3.0)
	cp.record_frames = false
	assert_eq(rolled.size(), 1, "one rollback")
	var inexact := 0
	for p: Vector2 in cp.seen_in_physics:
		if p.distance_to(before) > 0.002 and p.distance_to(target) > 0.002:
			inexact += 1
	assert_eq(inexact, 0, "_physics_process only ever sees simulated values (%d physics calls)" % cp.seen_in_physics.size())
	assert_gt(cp.seen_in_physics.size(), 60)
	var biggest := 0.0
	var blended := 0
	var prev: Vector2 = cp.seen_in_process[0] if cp.seen_in_process.size() > 0 else before
	for p: Vector2 in cp.seen_in_process:
		biggest = maxf(biggest, p.distance_to(prev))
		prev = p
		if p.x - before.x > 1.0 and p.x - before.x < 49.0:
			blended += 1
	print("rollback blend: largest rendered step %.2f px (the correction was 50 px), %d blended frames, %d physics calls" % [biggest, blended, cp.seen_in_physics.size()])
	assert_gt(blended, 5, "the correction was shown over several frames")
	assert_lt(biggest, 10.0, "no visible jump in _process")
	assert_lt(cp.position.distance_to(target), 0.002, "the shown position arrived")
	assert_gt(net.client_sessions[0].stats()["offset_corrections"], 0)


func test_cold_properties_still_snap() -> void:
	var managers := await _start()
	var copies := await _spawn(managers, MOVER, "C")
	var sm: Node2D = copies["Server"]
	var cm: Node2D = copies["ClientA"]
	cm.recording = true
	sm.modulate = Color(1, 0, 0, 1)
	assert_true(await net.wait_until(func() -> bool: return cm.modulate == Color(1, 0, 0, 1)), "the colour arrived")
	await _frames(5)
	cm.recording = false
	for c: Color in cm.shown_modulates:
		assert_true(c == Color(1, 1, 1, 1) or c == Color(1, 0, 0, 1), "a cold colour never blends: %s" % c)


func test_interpolate_false_in_configure_snaps() -> void:
	var probe: Node = MOVER_SNAP.instantiate()
	Net.configure(probe, {"interpolate": {"position": false}})
	probe.free()
	var managers := await _start()
	var copies := await _spawn(managers, MOVER_SNAP, "S")
	var sm: Node2D = copies["Server"]
	var cm: Node2D = copies["ClientA"]
	cm.recording = true
	sm.position.x += 20.0
	assert_true(await net.wait_until(func() -> bool: return cm.position.x == 20.0), "arrived")
	await _frames(5)
	cm.recording = false
	for p: Vector2 in cm.shown:
		assert_true(absf(p.x) < 0.001 or absf(p.x - 20.0) < 0.001, "snaps: %s" % p)


func test_teleport_distance_in_configure_lowers_the_threshold() -> void:
	var probe: Node = MOVER_TELEPORT.instantiate()
	Net.configure(probe, {"teleport_distance": {"position": 10.0}})
	probe.free()
	var managers := await _start()
	var copies := await _spawn(managers, MOVER_TELEPORT, "P")
	var sm: Node2D = copies["Server"]
	var cm: Node2D = copies["ClientA"]
	await _frames(20)
	cm.recording = true
	sm.position.x += 20.0
	assert_true(await net.wait_until(func() -> bool: return cm.position.x == 20.0), "arrived")
	await _frames(5)
	cm.recording = false
	for p: Vector2 in cm.shown:
		assert_true(absf(p.x) < 0.001 or absf(p.x - 20.0) < 0.001, "20 px is a teleport above 10: %s" % p)
	assert_gt(net.client_sessions[0].stats()["interp_teleports"], 0)


func test_deg_rotation_takes_the_short_way_across_the_seam() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var copies := await _spawn(managers, MOVER, "R")
	var sm: Node2D = copies["Server"]
	var cm: Node2D = copies["ClientA"]
	sm.rotation = 2.9
	assert_true(await net.wait_until(func() -> bool: return absf(cm.rotation - 2.9) < 0.001), "the start rotation arrived")
	cm.recording = true
	sm.spin = 1.5
	await wait_seconds(0.5)
	cm.recording = false
	sm.spin = 0.0
	var crossed_low := false
	var crossed_high := false
	var wrong := 0
	for i in cm.shown_rotations.size():
		var r: float = cm.shown_rotations[i]
		assert_gt(absf(r), 2.5, "never swings through zero: %f" % r)
		crossed_high = crossed_high or r > 3.0
		crossed_low = crossed_low or r < -3.0
		if i > 0:
			var d := wrapf(r - cm.shown_rotations[i - 1], -PI, PI)
			if d < -0.0001 or d > 0.2:
				wrong += 1
	assert_true(crossed_high and crossed_low, "the rotation crossed the seam")
	assert_eq(wrong, 0, "every frame turns a little the same way")


func test_the_jitter_buffer_grows_under_jitter_and_late_snapshots_fall() -> void:
	timeout_sec = 30.0
	assert_eq(Net.jitter_buffer_ticks, 2, "the default (D8)")
	var managers := await _start(1, NetFixture.HostMode.DEDICATED, _link(10.0, 40.0))
	var copies := await _spawn(managers, MOVER, "J")
	copies["Server"].speed = Vector2(60.0, 0.0)
	var s := net.client_sessions[0]
	var first: int = s.stats()["jitter_buffer_ticks"]
	await wait_seconds(4.0)
	var grown: int = s.stats()["jitter_buffer_ticks"]
	await wait_seconds(2.0)
	var late_a: int = s.stats()["late_snapshots_per_s"]
	await wait_seconds(1.0)
	var late_b: int = s.stats()["late_snapshots_per_s"]
	var st: Dictionary = s.stats()
	print("40 ms jitter: buffer %d -> %d ticks; late snapshots per second %d, %d (total %d); render lag %.2f ticks" % [first, grown, late_a, late_b, st["late_snapshots"], st["render_lag_ticks"]])
	assert_eq(first, 2)
	assert_gt(grown, 2, "the buffer grew")
	assert_lt(grown, 7)
	assert_lt(late_a, 3, "late snapshots fell under the threshold")
	assert_lt(late_b, 3)


func test_settings_and_the_session_surface() -> void:
	ProjectSettings.set_setting("gdnet/interpolation/jitter_buffer_ticks", 4)
	ProjectSettings.set_setting("gdnet/interpolation/adaptive", false)
	assert_eq(Net.jitter_buffer_ticks, 4, "the setting, without a session")
	var idle := NetSession.new()
	add_child(idle)
	await wait_frames(2)
	assert_false(idle.is_processing(), "gate G0: no render-frame work without a session")
	var managers := await _start()
	var copies := await _spawn(managers, MOVER, "K")
	copies["Server"].speed = Vector2(60.0, 0.0)
	await wait_seconds(1.5)
	var st: Dictionary = net.client_sessions[0].stats()
	assert_eq(st["jitter_buffer_ticks"], 4)
	assert_eq(net.client_sessions[0].jitter_buffer_ticks, 4)
	for k in ["render_lag_ticks", "late_snapshots_per_s", "offset_corrections_per_s"]:
		assert_true(st.has(k), k)
	assert_almost_eq(st["render_lag_ticks"], 4.0, 1.0)
	assert_true(net.client_sessions[0].is_processing())
	assert_false(net.server_session.is_processing(), "the server interpolates nothing")
