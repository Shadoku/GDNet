extends TestCase
## WP18 3D parity (docs/PLAN_NETCODE.md 12.5, gate G10): the 3D twin of interpolation (WP5), in both
## host modes (G2): a moving, turning replica is smooth and slerps across the quaternion double cover, a
## correction under 2 m blends and a teleport beyond it snaps, and a fast start is followed. Fixtures:
## tests/fixtures/net3d/.

const MOVER := preload("res://tests/fixtures/net3d/mover_3d.tscn")
## Half a `mm` step on three axes (0.0005 x sqrt(3)): the most a replica's position differs.
const HALF_STEP := 0.00087
## The most a `smallest3` round trip turns a rotation (radians; derived in test_net_3d.gd).
const ANGLE_BOUND := 1.5e-4

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null


func _frames(n: int) -> void:
	for i in n:
		await get_tree().physics_frame


## The rotation between two quaternions in radians, q and -q being one rotation.
func _angle(a: Quaternion, b: Quaternion) -> float:
	var d := a.inverse() * b
	return 2.0 * asin(minf(1.0, Vector3(d.x, d.y, d.z).length()))


# --- interpolation (WP5) --------------------------------------------------------------------------


## Adds `scene` named `node_name` under the server's manager; returns [server copy, client copy] once
## the client has it and ten ticks passed.
func _spawn_on(f: NetFixture, managers: Dictionary, scene: PackedScene, node_name: String) -> Array:
	var n: Node = scene.instantiate()
	n.name = node_name
	managers["Server"].add_child(n)
	var c: Node = f.clients[0]
	assert_true(await f.wait_until(func() -> bool: return c.has_node("World/" + node_name)), "the copy reached the client")
	await _frames(10)
	return [n, c.get_node("World/" + node_name)]


func test_interpolation_a_moving_turning_replica_is_smooth_and_slerps_across_the_double_cover() -> void:
	timeout_sec = 20.0
	var fps := Engine.max_fps
	Engine.max_fps = 144
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var pair := await _spawn_on(f, f.add_managers(), MOVER, "M")
		var sm: Node3D = pair[0]
		var cm: Node3D = pair[1]
		var axis := Vector3(0.3, 1.0, 0.2).normalized()
		# 2 rad about the axis: turning at 3 rad/s, it passes half a turn (w = 0, where a quaternion's
		# sign flips between q and -q in the quantiser) a third of a second into the recording.
		sm.quaternion = Quaternion(axis, 2.0)
		sm.axis = axis
		sm.speed = Vector3(1.2, 0.0, -0.6)
		sm.spin = 3.0
		await wait_seconds(0.3)
		cm.recording = true
		var f0 := Engine.get_process_frames()
		var t0 := Time.get_ticks_usec()
		await wait_seconds(1.2)
		cm.recording = false
		sm.spin = 0.0
		var achieved := float(Engine.get_process_frames() - f0) / (float(Time.get_ticks_usec() - t0) / 1e6)
		var dir: Vector3 = (sm.speed as Vector3).normalized()
		var steps: Array[float] = []
		var backwards := 0
		var worst_turn := 0.0
		var against := 0
		var crossed := false
		for i in range(1, cm.shown.size()):
			var d: float = (cm.shown[i] - cm.shown[i - 1]).dot(dir)
			steps.append(d)
			if d < -0.00001:
				backwards += 1
			var a: Quaternion = cm.shown_quaternions[i - 1]
			var b: Quaternion = cm.shown_quaternions[i]
			worst_turn = maxf(worst_turn, _angle(a, b))
			# The step's rotation axis points along the spin axis (never the other way round).
			var rel := b * a.inverse()
			if rel.w < 0.0:
				rel = -rel
			if Vector3(rel.x, rel.y, rel.z).dot(axis) < -0.00001:
				against += 1
			crossed = crossed or (signf(a.w) != signf(b.w))
		var mean := 0.0
		for d in steps:
			mean += d
		mean /= maxf(1.0, steps.size())
		var v := 0.0
		for d in steps:
			v += (d - mean) * (d - mean)
		var sd := sqrt(v / maxf(1.0, steps.size()))
		print("3D interpolation (mode %d, %.0f fps): per-frame displacement mean %.5f m, sd %.5f m; largest per-frame turn %.4f rad; quaternion w changed sign: %s" % [f.host_mode, achieved, mean, sd, worst_turn, crossed])
		assert_gt(cm.shown.size(), 30, "frames were recorded")
		assert_eq(backwards, 0, "the replica never moves backwards")
		assert_eq(against, 0, "it never turns backwards")
		assert_true(crossed, "the rotation crossed the double cover's seam (w changed sign)")
		# 3 rad/s is 0.05 rad a tick: a flip to the long way round would be a turn of about 2 PI.
		assert_lt(worst_turn, 0.1, "no frame turns by more than two ticks' worth")
		if achieved >= 144 * 0.85:
			assert_lt(sd, 0.25 * mean, "per-frame displacement is even")
		else:
			print("the headless runner reached %.1f of 144 frames per second: the evenness bound is not checked" % achieved))
	Engine.max_fps = fps
	assert_true(ok)


func test_interpolation_a_small_correction_blends_and_a_teleport_beyond_two_metres_snaps() -> void:
	timeout_sec = 20.0
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var pair := await _spawn_on(f, f.add_managers(), MOVER, "T")
		var sm: Node3D = pair[0]
		var cm: Node3D = pair[1]
		# 1.5 m is under the 2 m default (`gdnet/interpolation/teleport_distance` 0 = 2 m in 3D): it blends.
		cm.recording = true
		sm.position.x += 1.5
		assert_true(await f.wait_until(func() -> bool: return absf(cm.position.x - 1.5) < 0.01, 3.0), "arrived")
		cm.recording = false
		var between := 0
		var prev := 0.0
		for p: Vector3 in cm.shown:
			assert_lt(p.x, 1.5001, "never overshoots")
			assert_gt(p.x, prev - 0.00001, "approaches monotonically")
			prev = p.x
			if p.x > 0.05 and p.x < 1.45:
				between += 1
		assert_gt(between, 5, "a 1.5 m correction is spread over several frames")
		# 3 m is beyond it: one step.
		cm.clear_recording()
		cm.recording = true
		sm.position.x += 3.0
		assert_true(await f.wait_until(func() -> bool: return absf(cm.position.x - 4.5) < 0.001, 2.0), "arrived")
		await _frames(3)
		cm.recording = false
		for p: Vector3 in cm.shown:
			assert_true(p.x < 1.5001 or p.x > 4.49, "a teleport snaps: %s" % p)
		assert_gt(f.session(f.clients[0]).stats()["interp_teleports"], 0))
	assert_true(ok)


## WP18: a replica that starts moving faster than the correction epsilon per tick (5 cm, 5 degrees)
## follows at once: one correction when it starts, then the stream's heading (before WP18 every sample
## was a correction and the replica lagged by whole turns).
func test_interpolation_a_fast_start_is_followed_not_corrected_every_tick() -> void:
	var ok: bool = await NetFixture.run_both_modes(self, 1, func(f: NetFixture) -> void:
		var pair := await _spawn_on(f, f.add_managers(), MOVER, "F")
		var sm: Node3D = pair[0]
		var cm: Node3D = pair[1]
		var s := f.session(f.clients[0])
		var before: int = s.stats()["offset_corrections"]
		# 6 m/s and 12 rad/s: 0.1 m and 0.2 rad a tick.
		sm.axis = Vector3(0.3, 1.0, 0.2).normalized()
		sm.speed = Vector3(6.0, 0.0, 0.0)
		sm.spin = 12.0
		await _frames(30)
		var corrections: int = s.stats()["offset_corrections"] - before
		var buffer: int = s.stats()["jitter_buffer_ticks"]
		# The replica shows the server's motion about a jitter buffer plus a tick of transit late.
		var lag_ticks := (sm.position.x - cm.position.x) / 0.1
		print("fast start (mode %d): %d corrections; the replica %.2f ticks behind (jitter buffer %d)" % [f.host_mode, corrections, lag_ticks, buffer])
		assert_lt(corrections, 3, "one correction per field when it starts (position, rotation), not one per tick")
		assert_lt(lag_ticks, buffer + 3.0, "the replica keeps up")
		sm.speed = Vector3.ZERO
		sm.spin = 0.0
		assert_true(await f.wait_until(func() -> bool:
			return cm.position.distance_to(sm.position) < HALF_STEP and _angle(cm.quaternion, sm.quaternion) < ANGLE_BOUND, 2.0), "and settles where the server stopped"))
	assert_true(ok)
