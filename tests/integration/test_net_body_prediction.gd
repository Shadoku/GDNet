extends "res://tests/fixtures/predict/prediction_case.gd"
## WP19 body prediction (docs/PLAN_NETCODE.md 12.4 "Body prediction", gate G13, D26, D27), 2D: a frozen
## `RigidBody2D` replica shows the server's newest snapshot carried forward to the present
## (`dead_reckoning`), or the game's `_net_predict_body(delta)` result (`custom`), instead of the
## interpolated past (`interpolate`, the default). Gate G13 in both host modes, the hook, a bounce,
## rest and the resolution order; test_net_body_prediction_motion.gd has the cap, smoothness, a
## change in flight and a predicted player; the helpers are in tests/fixtures/predict/prediction_case.gd.


# --- gate G13 ---------------------------------------------------------------------------------


func test_g13_a_dead_reckoned_ball_in_flight_stays_within_a_tick_of_the_present() -> void:
	timeout_sec = 30.0
	for mode: NetFixture.HostMode in [NetFixture.HostMode.HOST_PLAYER, NetFixture.HostMode.DEDICATED]:
		var managers := await _start(50.0, mode)
		var a := await _spawn(managers, BALL, "A", Vector2(0.0, 0.0))
		var b := await _spawn(managers, BALL, "B", Vector2(0.0, 400.0))
		# The client's choice, on its replica: nothing changes on the wire or on the server.
		Net.configure(a[1], {"body_prediction": "dead_reckoning"})
		assert_eq(Net.body_prediction_of(a[1]), NetEntity.BODY_PREDICTION_DEAD_RECKONING)
		assert_eq(Net.body_prediction_of(b[1]), NetEntity.BODY_PREDICTION_INTERPOLATE, "the default (D27)")
		await wait_seconds(0.8)
		a[0].launch(THROW)
		b[0].launch(THROW)
		await wait_seconds(0.25)
		await _record([a[1], b[1]], 0.6)
		var dr := _g13(a[1], 0.05)
		var ip := _g13(b[1], 0.05)
		var s := _stats()
		print("G13 2D (%s): dead reckoning over %d frames: worst %.2f px against one tick's travel %.2f px (excess %.2f), mean %.2f px, lead %.2f ticks (%.2f..%.2f); interpolated: mean trail %.1f px = %.2f x velocity x one-way delay; horizon %.1f ms, predicted bodies %d, corrections/s %d" % ["host" if mode == NetFixture.HostMode.HOST_PLAYER else "dedicated", dr["frames"], dr["worst"], dr["travel"], dr["excess"], dr["mean"], dr["lead"], dr["lead_min"], dr["lead_max"], ip["mean"], ip["trail"], s["prediction_horizon_ms"], s["predicted_bodies"], s["prediction_corrections_per_s"]])
		assert_gt(dr["frames"], 20, "frames were recorded")
		assert_lt(dr["excess"], 0.05, "G13: within one tick's travel of the server's present position on every frame")
		assert_gt(ip["trail"], 0.5, "an interpolated replica trails by more than half its velocity times the one-way delay")
		assert_eq(s["predicted_bodies"], 1, "one body predicted")
		assert_gt(s["prediction_horizon_ms"], 40.0, "the horizon covers at least the one-way delay")
		var server_stats: Dictionary = net.server_session.stats()
		assert_eq(server_stats["predicted_bodies"], 0, "the server predicts nothing")
		net.teardown()
		net = null
		await wait_frames(2)


# --- custom -----------------------------------------------------------------------------------


func test_a_hook_is_what_the_replica_shows() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var t := await _spawn(managers, THRUST, "T", Vector2(0.0, 0.0))
	var d := await _spawn(managers, THRUST, "D", Vector2(0.0, 300.0))
	var o := await _spawn(managers, ODD, "O", Vector2(0.0, 600.0))
	var ti := await _spawn(managers, THRUST_INTERP, "TI", Vector2(0.0, 900.0))
	# A hook and no setting: custom. An explicit setting wins over the hook.
	Net.configure(d[1], {"body_prediction": "dead_reckoning"})
	assert_eq(Net.body_prediction_of(t[1]), NetEntity.BODY_PREDICTION_CUSTOM)
	assert_eq(Net.body_prediction_of(o[1]), NetEntity.BODY_PREDICTION_CUSTOM)
	assert_eq(Net.body_prediction_of(d[1]), NetEntity.BODY_PREDICTION_DEAD_RECKONING)
	assert_eq(Net.body_prediction_of(ti[1]), NetEntity.BODY_PREDICTION_INTERPOLATE, "the NetEntity's explicit interpolate wins")
	await wait_seconds(0.8)
	for p: Array in [t, d, o, ti]:
		p[0].launch(Vector2(0.0, -300.0))
	await wait_seconds(0.25)
	var f0 := Engine.get_process_frames()
	var calls0: int = t[1].hook_calls
	await _record([t[1], d[1], o[1]], 0.5)
	var frames := Engine.get_process_frames() - f0
	var hook := _g13(t[1], 0.05)
	var plain := _g13(d[1], 0.05)
	var h: float = _stats()["prediction_horizon_ms"] / 1000.0
	var model: float = 0.5 * 2400.0 * h * h
	var per_frame := float(t[1].hook_calls - calls0) / maxf(1.0, float(frames))
	print("custom 2D: with the hook worst %.2f px against one tick's travel %.2f, mean %.2f px; dead reckoning without it mean %.2f px (the thrust over the horizon: %.2f px); hook calls per frame %.2f (horizon %.1f ms)" % [hook["worst"], hook["travel"], hook["mean"], plain["mean"], model, per_frame, h * 1000.0])
	assert_lt(hook["excess"], 0.05, "the hook's thrust keeps the replica within one tick's travel of the present")
	assert_gt(plain["mean"] - hook["mean"], 0.5 * model, "without the hook the thrust over the horizon is missing")
	assert_gt(per_frame, 0.5, "the hook ran every frame")
	assert_lt(per_frame, 4.0, "a few calls per frame: whole ticks once, one fraction per frame, a re-base per snapshot")
	assert_eq(ti[1].hook_calls, 0, "an interpolated body's hook never runs")
	var spots: PackedVector2Array = o[1].shown
	var rotations: PackedFloat64Array = o[1].shown_rotations
	assert_gt(spots.size(), 20)
	for i in spots.size():
		if spots[i] != Vector2(1234.0, -1234.0) or not is_equal_approx(rotations[i], 1.0):
			fail("frame %d shows %s / %.3f rad, not the hook's spot" % [i, spots[i], rotations[i]])
			return


# --- corrections ------------------------------------------------------------------------------


func test_a_misprediction_at_a_bounce_bends_back_and_ends_where_the_server_is() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	_floor(200.0)
	var b := await _spawn(managers, BOUNCY, "B", Vector2(0.0, 0.0))
	Net.configure(b[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.8)
	b[0].launch(Vector2(0.0, 500.0))
	# A body that starts moving starts at the prediction (one jump of its velocity times the horizon);
	# the bounce comes about 0.28 s after the launch.
	await wait_seconds(0.1)
	await _record([b[1]], 1.5)
	var rest_y := 200.0 - 8.0
	var shown: PackedVector2Array = b[1].shown
	var speeds: PackedFloat64Array = b[1].other_speed
	var over := 0.0
	var v_max := 0.0
	var step_max := 0.0
	for i in shown.size():
		over = maxf(over, shown[i].y - rest_y)
		v_max = maxf(v_max, speeds[i])
		if i > 0:
			step_max = maxf(step_max, shown[i].distance_to(shown[i - 1]))
	var cap_s := 0.25
	var s := _stats()
	print("bounce 2D: passed the floor by %.1f px (impact speed %.0f px/s; speed x 250 ms cap = %.1f px); largest per-frame step %.2f px (one tick's travel at impact %.2f px); corrections %d" % [over, v_max, v_max * cap_s, step_max, v_max / 60.0, s["prediction_corrections"]])
	assert_gt(over, 0.0, "dead reckoning passes into the floor before the bounce arrives")
	assert_lt(over, v_max * cap_s, "by less than the impact speed times the horizon cap")
	assert_lt(step_max, 2.0 * v_max / 60.0, "it bends back without a jump: no frame moves more than two ticks of the impact speed")
	assert_gt(s["prediction_corrections"], 0, "the bounce was a correction")
	# At rest: exactly where the server (and interpolation) has it.
	assert_true(await net.wait_until(func() -> bool: return b[0].sleeping, 5.0), "the server's ball fell asleep")
	await wait_seconds(0.2)
	assert_lt((b[1] as Node2D).position.distance_to(b[0].position), HALF_STEP, "the replica ends where the server's ball is")


# --- rest, cost and resolution ----------------------------------------------------------------


func test_a_sleeping_pile_is_not_predicted() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	(managers["ClientA"] as NetManager).body_prediction = NetEntity.BODY_PREDICTION_DEAD_RECKONING
	_floor(200.0)
	var pile: Array = []
	for i in 6:
		var b: RigidBody2D = BALL.instantiate()
		b.freeze = false
		b.name = "P%d" % i
		b.position = Vector2(20.0 * i, 200.0 - 8.0)
		(managers["Server"] as Node).add_child(b)
		pile.append(b)
	var c: Node = net.clients[0]
	assert_true(await net.wait_until(func() -> bool:
		for b: RigidBody2D in pile:
			if not b.sleeping or not c.has_node("World/" + String(b.name)):
				return false
		return true, 5.0), "the pile fell asleep")
	await wait_seconds(0.3)
	for i in 20:
		await get_tree().process_frame
		assert_eq(_stats()["predicted_bodies"], 0, "a sleeping pile is not predicted (frame %d)" % i)
	for b: RigidBody2D in pile:
		var r: RigidBody2D = c.get_node("World/" + String(b.name))
		assert_eq(Net.body_prediction_of(r), NetEntity.BODY_PREDICTION_DEAD_RECKONING)
		assert_lt(r.position.distance_to(b.position), HALF_STEP, "where interpolation puts it: the server's position")


func test_resolution_order_and_changes_at_run_time() -> void:
	timeout_sec = 15.0
	var managers := await _start(0.0)
	var p := await _spawn(managers, BALL, "P", Vector2(0.0, 0.0))
	var r := await _spawn(managers, BALL_RECKON, "R", Vector2(40.0, 0.0))
	var t := await _spawn(managers, THRUST, "T", Vector2(80.0, 0.0))
	var ti := await _spawn(managers, THRUST_INTERP, "TI", Vector2(120.0, 0.0))
	var m: NetManager = managers["ClientA"]
	var I := NetEntity.BODY_PREDICTION_INTERPOLATE
	var D := NetEntity.BODY_PREDICTION_DEAD_RECKONING
	var C := NetEntity.BODY_PREDICTION_CUSTOM
	var of := func() -> Array:
		return [Net.body_prediction_of(p[1]), Net.body_prediction_of(r[1]), Net.body_prediction_of(t[1]), Net.body_prediction_of(ti[1])]
	assert_eq(of.call(), [I, D, C, I], "project default interpolate; the entity's setting; a hook; an explicit interpolate over a hook")
	m.body_prediction = D
	assert_eq(of.call(), [D, D, C, I], "the manager's default; a hook stays custom")
	m.body_prediction = NetEntity.BODY_PREDICTION_DEFAULT
	ProjectSettings.set_setting("gdnet/physics/body_prediction", "dead_reckoning")
	assert_eq(of.call(), [D, D, C, I], "the project's default")
	ProjectSettings.set_setting("gdnet/physics/body_prediction", "custom")
	assert_eq(of.call(), [D, D, C, I], "custom without a hook is dead reckoning")
	ProjectSettings.set_setting("gdnet/physics/body_prediction", null)
	Net.configure(r[1], {"body_prediction": "interpolate"})
	assert_eq(Net.body_prediction_of(r[1]), I, "configure() over the NetEntity")
	Net.configure(r[1], {"body_prediction": "default"})
	assert_eq(Net.body_prediction_of(r[1]), D, "back to the NetEntity")
	(ti[1].get_node("NetEntity") as NetEntity).body_prediction = NetEntity.BODY_PREDICTION_DEFAULT
	assert_eq(Net.body_prediction_of(ti[1]), C, "the NetEntity changed at run time: the hook decides")
	# G0: without a session, and on the server's own copies, nothing is predicted.
	var loose: RigidBody2D = BALL.instantiate()
	add_child(loose)
	assert_eq(Net.body_prediction_of(loose), I)
	assert_eq(net.server_session.stats()["predicted_bodies"], 0)
	# The overlay row names a predicted body's mode (resolved at the next rendered frame).
	await wait_frames(2)
	var modes := {}
	for row: Dictionary in net.client_sessions[0].overlay_rows():
		modes[row["node"]] = row["body_prediction"]
	assert_eq(modes.get(r[1]), "dead_reckoning")
	assert_eq(modes.get(p[1]), "interpolate")
