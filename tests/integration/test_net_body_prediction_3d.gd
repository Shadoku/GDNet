extends TestCase
## WP19 body prediction in 3D (docs/PLAN_NETCODE.md 12.4, gate G13): the twins of
## test_net_body_prediction.gd for `RigidBody3D` (metres, quaternions): free flight against the server's
## present, the 12.4 example hook, a bounce on the floor and the resolution order. 50 ms each way.

const BALL := preload("res://tests/fixtures/predict/ball_3d.tscn")
const BOUNCY := preload("res://tests/fixtures/predict/bouncy_ball_3d.tscn")
const SHIP := preload("res://tests/fixtures/predict/ship_3d.tscn")
const FLOOR := preload("res://tests/fixtures/net3d/floor_3d.tscn")
const CLIENT_LAYER := 1 << 4
## Half a `mm` step on three axes (0.0005 x sqrt(3)).
const HALF_STEP := 0.00087
## A thrown ball: 3 m/s along x, 4 m/s up, 2 rad/s about z; gravity 9.8 m/s^2.
const THROW := Vector3(3.0, 4.0, 1.0)
const SPIN := Vector3(0.0, 0.0, 2.0)

var net: NetFixture


func after_each() -> void:
	if net != null:
		net.teardown()
		net = null
	if ProjectSettings.has_setting("gdnet/physics/body_prediction"):
		ProjectSettings.set_setting("gdnet/physics/body_prediction", null)


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 23
	return e


func _start(mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED) -> Dictionary:
	net = NetFixture.new(self, 1, mode)
	for c in net.clients:
		net.set_emulation(c, _link(50.0))
	net.start()
	assert_true(await net.wait_connected(10.0))
	var managers := net.add_managers()
	for c in net.clients:
		(managers[String(c.name)] as Node).child_entered_tree.connect(func(n: Node) -> void:
			if n is RigidBody3D:
				(n as RigidBody3D).collision_layer = CLIENT_LAYER
				(n as RigidBody3D).collision_mask = 0)
	return managers


func _spawn(managers: Dictionary, scene: PackedScene, node_name: String, at: Vector3) -> Array:
	var n: Node3D = scene.instantiate()
	n.name = node_name
	n.position = at
	(managers["Server"] as Node).add_child(n)
	var c: Node = net.clients[0]
	var path := "World/" + node_name
	assert_true(await net.wait_until(func() -> bool: return c.has_node(path), 10.0), "the copy reached the client")
	var r: Node3D = c.get_node(path)
	r.set("other", n)
	return [n, r]


func _angle(a: Quaternion, b: Quaternion) -> float:
	var d := a.inverse() * b
	return 2.0 * asin(minf(1.0, Vector3(d.x, d.y, d.z).length()))


func _record(bodies: Array, seconds: float) -> void:
	for b: Node in bodies:
		b.call("clear_recording")
		b.set("recording", true)
	await wait_seconds(seconds)
	for b: Node in bodies:
		b.set("recording", false)


## The worst frame's error minus one tick's travel, the worst error, the mean error, the mean trail in
## units of `velocity x one-way delay`, the worst rotation error (radians).
func _g13(r: Node, one_way_s: float) -> Dictionary:
	var shown: PackedVector3Array = r.get("shown")
	var other: PackedVector3Array = r.get("other_shown")
	var speed: PackedFloat64Array = r.get("other_speed")
	var rots: Array = r.get("shown_rotations")
	var other_rots: Array = r.get("other_rotations")
	var excess := -INF
	var worst := 0.0
	var travel := 0.0
	var sum := 0.0
	var trail := 0.0
	var angle := 0.0
	var tps := float(Engine.physics_ticks_per_second)
	for i in shown.size():
		var e := shown[i].distance_to(other[i])
		var t := speed[i] / tps
		if e - t > excess:
			excess = e - t
			worst = e
			travel = t
		sum += e
		trail += e / maxf(1e-6, speed[i] * one_way_s)
		angle = maxf(angle, _angle(rots[i], other_rots[i]))
	var n := maxf(1.0, float(shown.size()))
	return {"excess": excess, "worst": worst, "travel": travel, "mean": sum / n, "trail": trail / n, "angle": angle, "frames": shown.size()}


func test_g13_a_dead_reckoned_ball_in_flight_stays_within_a_tick_of_the_present_3d() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var a := await _spawn(managers, BALL, "A", Vector3(0.0, 2.0, 0.0))
	var b := await _spawn(managers, BALL, "B", Vector3(0.0, 2.0, 5.0))
	(managers["ClientA"] as NetManager).body_prediction = NetEntity.BODY_PREDICTION_DEAD_RECKONING
	Net.configure(b[1], {"body_prediction": "interpolate"})
	assert_eq(Net.body_prediction_of(a[1]), NetEntity.BODY_PREDICTION_DEAD_RECKONING)
	assert_eq(Net.body_prediction_of(b[1]), NetEntity.BODY_PREDICTION_INTERPOLATE)
	await wait_seconds(0.8)
	a[0].launch(THROW, SPIN)
	b[0].launch(THROW, SPIN)
	await wait_seconds(0.25)
	await _record([a[1], b[1]], 0.6)
	var dr := _g13(a[1], 0.05)
	var ip := _g13(b[1], 0.05)
	var s: Dictionary = net.client_sessions[0].stats()
	print("G13 3D: dead reckoning over %d frames: worst %.4f m against one tick's travel %.4f m (excess %.4f), mean %.4f m, worst rotation error %.4f rad; interpolated: mean trail %.3f m = %.2f x velocity x one-way delay, worst rotation error %.3f rad; horizon %.1f ms" % [dr["frames"], dr["worst"], dr["travel"], dr["excess"], dr["mean"], dr["angle"], ip["mean"], ip["trail"], ip["angle"], s["prediction_horizon_ms"]])
	assert_gt(dr["frames"], 20, "frames were recorded")
	assert_lt(dr["excess"], 0.0005, "G13: within one tick's travel of the server's present position on every frame")
	assert_lt(dr["angle"], 2.0 * 2.0 / 60.0, "the rotation within two ticks of the spin")
	assert_gt(ip["trail"], 0.5, "an interpolated replica trails by more than half its velocity times the one-way delay")


func test_the_12_4_example_hook_works() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var s := await _spawn(managers, SHIP, "S", Vector3(0.0, 1.0, 0.0))
	assert_eq(Net.body_prediction_of(s[1]), NetEntity.BODY_PREDICTION_CUSTOM, "a hook and no setting")
	s[0].throttle = 1.0
	await wait_seconds(0.8)
	assert_eq(s[1].throttle, 1.0, "the throttle replicated")
	s[0].launch(Vector3(0.0, 0.0, -0.5))
	await wait_seconds(0.25)
	await _record([s[1]], 0.6)
	var e := _g13(s[1], 0.05)
	print("12.4 hook 3D: worst %.4f m against one tick's travel %.4f m, mean %.4f m" % [e["worst"], e["travel"], e["mean"]])
	assert_lt(e["excess"], 0.0005, "the example hook keeps the thrusting ship within one tick's travel of the present")


func test_a_bounce_bends_back_and_ends_where_the_server_is_3d() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	net.server.add_child(FLOOR.instantiate())
	var b := await _spawn(managers, BOUNCY, "B", Vector3(0.0, 2.0, 0.0))
	Net.configure(b[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.8)
	b[0].launch(Vector3(0.0, -5.0, 0.0))
	# A body that starts moving starts at the prediction; the bounce comes about 0.3 s after the launch.
	await wait_seconds(0.1)
	await _record([b[1]], 1.5)
	var shown: PackedVector3Array = b[1].shown
	var speeds: PackedFloat64Array = b[1].other_speed
	var over := 0.0
	var v_max := 0.0
	var step_max := 0.0
	for i in shown.size():
		over = maxf(over, 0.1 - shown[i].y)
		v_max = maxf(v_max, speeds[i])
		if i > 0:
			step_max = maxf(step_max, shown[i].distance_to(shown[i - 1]))
	print("bounce 3D: passed the floor by %.3f m (impact speed %.2f m/s; speed x 250 ms = %.3f m); largest per-frame step %.4f m (one tick's travel at impact %.4f m)" % [over, v_max, v_max * 0.25, step_max, v_max / 60.0])
	assert_gt(over, 0.0, "dead reckoning passes into the floor before the bounce arrives")
	assert_lt(over, v_max * 0.25, "by less than the impact speed times the horizon cap")
	assert_lt(step_max, 2.0 * v_max / 60.0, "it bends back without a jump")
	assert_true(await net.wait_until(func() -> bool: return b[0].sleeping, 5.0), "the server's ball fell asleep")
	await wait_seconds(0.2)
	assert_lt((b[1] as Node3D).position.distance_to(b[0].position), HALF_STEP, "the replica ends where the server's ball is")


func test_resolution_and_g0_in_3d() -> void:
	var managers := await _start(NetFixture.HostMode.HOST_PLAYER)
	var a := await _spawn(managers, BALL, "A", Vector3.ZERO)
	var s := await _spawn(managers, SHIP, "S", Vector3(1.0, 0.0, 0.0))
	assert_eq(Net.body_prediction_of(a[1]), NetEntity.BODY_PREDICTION_INTERPOLATE)
	ProjectSettings.set_setting("gdnet/physics/body_prediction", "dead_reckoning")
	assert_eq(Net.body_prediction_of(a[1]), NetEntity.BODY_PREDICTION_DEAD_RECKONING)
	assert_eq(Net.body_prediction_of(s[1]), NetEntity.BODY_PREDICTION_CUSTOM)
	Net.configure(s[1], {"body_prediction": "interpolate"})
	assert_eq(Net.body_prediction_of(s[1]), NetEntity.BODY_PREDICTION_INTERPOLATE)
	assert_eq(net.server_session.stats()["predicted_bodies"], 0, "the host predicts nothing")
