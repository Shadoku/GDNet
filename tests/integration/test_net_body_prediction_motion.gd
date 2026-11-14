extends "res://tests/fixtures/predict/prediction_case.gd"
## WP19 body prediction, 2D, continued from test_net_body_prediction.gd: the horizon cap over a slow
## link, a change of mode in flight, smooth motion at 60 and 144 fps, and a predicted player (WP4)
## meeting a dead-reckoned body where it is now.


# --- the cap ----------------------------------------------------------------------------------


func test_the_horizon_is_capped() -> void:
	# 300 ms each way: the newest snapshot is about 300 ms old when it arrives; the cap is 250 ms.
	timeout_sec = 25.0
	var managers := await _start(300.0)
	var a := await _spawn(managers, PUCK, "A", Vector2(0.0, 0.0))
	Net.configure(a[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(1.5)
	a[0].launch(Vector2(400.0, 0.0))
	await wait_seconds(0.5)
	var horizons: Array = []
	for i in 30:
		await get_tree().process_frame
		horizons.append(_stats()["prediction_horizon_ms"])
	await _record([a[1]], 0.4)
	var e := _g13(a[1], 0.3)
	var trail_s: float = e["mean"] / 400.0
	var tick_ms := 1000.0 / Engine.physics_ticks_per_second
	print("cap: horizon %.1f..%.1f ms; the replica trails the present by %.1f px = %.0f ms of travel (a 300 ms one-way link)" % [horizons.min(), horizons.max(), e["mean"], trail_s * 1000.0])
	# The cap holds at each snapshot; between two snapshots the horizon runs on with the clock.
	assert_gt(horizons.min(), 249.5, "capped, not shortened")
	assert_lt(horizons.max(), 250.0 + tick_ms + 1.0, "the horizon stops at gdnet/physics/predict_max_ms (plus the time since the last snapshot)")
	assert_gt(trail_s, 0.025, "the replica trails the present by the excess (at least 300 - 250 ms of travel)")
	assert_lt(trail_s, 0.1, "and not by more than the excess plus a snapshot's age")


# --- changes and smoothness -------------------------------------------------------------------


func test_a_change_at_run_time_on_the_client_takes_effect_in_flight() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var a := await _spawn(managers, PUCK, "A", Vector2(0.0, 0.0))
	await wait_seconds(0.8)
	a[0].launch(Vector2(300.0, 0.0))
	await wait_seconds(0.2)
	await _record([a[1]], 0.3)
	var before := _g13(a[1], 0.05)
	Net.configure(a[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.4)
	await _record([a[1]], 0.3)
	var after := _g13(a[1], 0.05)
	print("run-time change: interpolated trail %.1f px, then dead reckoning worst %.2f px (tick travel %.2f)" % [before["mean"], after["worst"], after["travel"]])
	assert_gt(before["trail"], 0.5, "interpolated first: behind")
	assert_lt(after["excess"], 0.05, "dead reckoning after the change: at the present")


func test_smooth_at_60_and_144_fps() -> void:
	timeout_sec = 25.0
	var managers := await _start()
	var a := await _spawn(managers, PUCK, "A", Vector2(0.0, 0.0))
	Net.configure(a[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.8)
	a[0].launch(Vector2(240.0, 0.0))
	for fps: int in [60, 144]:
		Engine.max_fps = fps
		await wait_seconds(0.25)
		var f0 := Engine.get_process_frames()
		var t0 := Time.get_ticks_usec()
		await _record([a[1]], 0.8)
		var achieved := float(Engine.get_process_frames() - f0) / (float(Time.get_ticks_usec() - t0) / 1e6)
		var shown: PackedVector2Array = a[1].shown
		var steps: Array = []
		var backwards := 0
		for i in range(1, shown.size()):
			var dx: float = shown[i].x - shown[i - 1].x
			steps.append(dx)
			if dx < -0.001:
				backwards += 1
		var mean := _mean(steps)
		var v := 0.0
		for x: float in steps:
			v += (x - mean) * (x - mean)
		v /= maxf(1.0, float(steps.size()))
		var e := _g13(a[1], 0.05)
		print("smooth %d fps (achieved %.1f): per-frame displacement mean %.3f px, sd %.3f px; worst error %.2f px (tick travel %.2f), lead %.2f ticks (%.2f..%.2f)" % [fps, achieved, mean, sqrt(v), e["worst"], e["travel"], e["lead"], e["lead_min"], e["lead_max"]])
		assert_eq(backwards, 0, "never backwards at %d fps" % fps)
		assert_lt(e["excess"], 0.05, "at the present at %d fps" % fps)
		if fps > Engine.physics_ticks_per_second:
			# Rendered faster than the physics ticks, the frames sample every phase of the server's
			# stepwise position, so the mean signed error is the estimate's bias: centred, the worst
			# frame is half a tick off and G13 holds with half a tick to spare. (A round trip in
			# milliseconds put the estimate 0.23 to 0.55 of a tick behind, changing from run to run.)
			assert_lt(absf(e["lead"]), 0.2, "the present estimate is centred at %d fps: %.2f ticks" % [fps, e["lead"]])
		if achieved < fps * 0.85:
			print("the headless runner reached %.1f of %d frames per second: the variance bound is skipped" % [achieved, fps])
		else:
			assert_lt(sqrt(v), 0.25 * mean, "even per-frame displacement at %d fps" % fps)


# --- a predicted player meets bodies where they are now ---------------------------------------


func test_a_predicted_player_meets_a_dead_reckoned_body_where_it_is_now() -> void:
	timeout_sec = 20.0
	var managers := await _start()
	var peer: int = net.peer_id(net.clients[0])
	var c: Node = net.clients[0]
	var walkers: Array = []
	for y: float in [0.0, 100.0]:
		var w_name := "W%d" % int(y)
		# Away from the origin: a replica enters the tree there before its state is applied.
		(Net.spawn_for(peer, WALKER, managers["Server"], w_name) as Node2D).position = Vector2(100.0, y)
		assert_true(await net.wait_until(func() -> bool: return c.has_node("World/" + w_name)))
		walkers.append(c.get_node("World/" + w_name))
	var dr := await _spawn(managers, PUCK, "DR", Vector2(450.0, 0.0))
	var ip := await _spawn(managers, PUCK, "IP", Vector2(450.0, 100.0))
	Net.configure(dr[1], {"body_prediction": "dead_reckoning"})
	walkers[0].reference = dr[0]
	walkers[1].reference = ip[0]
	await wait_seconds(1.0)
	dr[0].launch(Vector2(-300.0, 0.0))
	ip[0].launch(Vector2(-300.0, 0.0))
	Input.action_press("right")
	assert_true(await net.wait_until(func() -> bool: return walkers[0].touched != null and walkers[1].touched != null, 5.0), "both walkers met their puck")
	Input.action_release("right")
	var e_dr: float = walkers[0].touched_at.distance_to(walkers[0].reference_at)
	var e_ip: float = walkers[1].touched_at.distance_to(walkers[1].reference_at)
	print("contact: a dead-reckoned puck was %.1f px from the server's present puck when the predicted player met it; an interpolated one %.1f px" % [e_dr, e_ip])
	assert_eq(walkers[0].touched, dr[1], "the client's player met the client's replica")
	assert_lt(e_dr, 0.5 * e_ip, "dead reckoning: the player meets the body much closer to where it is now")
