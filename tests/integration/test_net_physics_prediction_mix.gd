extends "res://tests/fixtures/predict/prediction_case.gd"
## WP19 slices A and B together (docs/PLAN_NETCODE.md 12.4): body prediction (`body_prediction`) and the
## physics modes (`physics`). gdnet predicts or interpolates only the bodies a peer shows as frozen
## replicas: a `simulated` replica is never predicted, an `owner` body is predicted on the other clients
## and never on its owner, and a mode or owner change moves a body into and out of prediction without a
## jump and without a stale entry. A body the game froze stays frozen on the peer that simulates it.
## One mixed scene (a dead-reckoned `server` ball, an `owner` vehicle a client drives, a `simulated`
## crate) over a 100 ms link, and the shared `correction_*` settings read in metres on both sides.
##
## The branches share one physics space: every body entering a branch's manager moves to that branch's
## own collision layer (bit 1 + branch index; the floor is layer 1), so copies never touch.

const CRATE := preload("res://tests/fixtures/physics/crate.tscn")
const PHYSICS_FLOOR := preload("res://tests/fixtures/physics/floor.tscn")
const VEHICLE := preload("res://tests/fixtures/physics/vehicle.tscn")
const BALL_3D := preload("res://tests/fixtures/predict/ball_3d.tscn")
const NONE := NetEntity.BODY_PREDICTION_NONE
const DR := NetEntity.BODY_PREDICTION_DEAD_RECKONING
## The puck's speed: no gravity, no damping, so it keeps it on every peer that simulates it.
const PUCK_SPEED := 150.0


## Records, after everything else in the frame (gdnet writes replicas in the session's `_process`),
## the position and speed of each watched body and the frame's time.
class Probe extends Node:
	var nodes: Array = []
	var pos: Array = []
	var speed: Array = []
	var times: Array = []
	var on: bool = false

	func _init() -> void:
		process_priority = 1000

	func watch(list: Array) -> void:
		nodes = list
		pos = []
		speed = []
		times = []
		for n: Node in list:
			pos.append([])
			speed.append([])

	func _process(_delta: float) -> void:
		if not on:
			return
		times.append(Time.get_ticks_usec())
		for i in nodes.size():
			var n: Node = nodes[i]
			pos[i].append(n.get("position"))
			speed[i].append((n.get("linear_velocity") as Vector2).length() if n is RigidBody2D else (n.get("linear_velocity") as Vector3).length())


var probe: Probe


func before_each() -> void:
	super.before_each()
	probe = Probe.new()
	add_child(probe)


## `n_clients` clients over a `ms` each-way link; managers under `World`; per-branch collision layers.
func _start_mix(n_clients: int, ms: float = 50.0) -> Dictionary:
	net = NetFixture.new(self, n_clients)
	for c in net.clients:
		net.set_emulation(c, _link(ms))
	net.start()
	assert_true(await net.wait_connected(10.0))
	var managers := net.add_managers()
	var branches: Array[Node] = [net.server]
	branches.append_array(net.clients)
	for k in branches.size():
		var bit := 1 << (1 + k)
		(managers[String(branches[k].name)] as Node).child_entered_tree.connect(func(n: Node) -> void:
			if n is RigidBody2D:
				(n as CollisionObject2D).collision_layer = bit
				(n as CollisionObject2D).collision_mask = 1 | bit)
	return managers


## Adds `scene` at `at` under the server's manager; returns [server copy, copy on each client...].
func _add(managers: Dictionary, scene: PackedScene, node_name: String, at: Vector2, setup: Callable = Callable()) -> Array:
	var n: Node2D = scene.instantiate()
	n.name = node_name
	n.position = at
	if setup.is_valid():
		setup.call(n)
	(managers["Server"] as Node).add_child(n)
	var out: Array = [n]
	for c: Node in net.clients:
		var path := "World/" + node_name
		assert_true(await net.wait_until(func() -> bool: return c.has_node(path), 10.0), "%s reached %s" % [node_name, c.name])
		var r: Node = c.get_node(path)
		r.set("other", n)  # the ball fixtures record the server's copy beside their own pose
		out.append(r)
	return out


## How far a body's collider (the physics server's transform) is from where the node is.
func _collider_gap(b: RigidBody2D) -> float:
	var t: Transform2D = PhysicsServer2D.body_get_state(b.get_rid(), PhysicsServer2D.BODY_STATE_TRANSFORM)
	return t.origin.distance_to(b.global_position)


func _cstats(i: int) -> Dictionary:
	return net.client_sessions[i].stats()


func _overlay_mode(i: int, node: Node) -> String:
	for row: Dictionary in net.client_sessions[i].overlay_rows():
		if row["node"] == node:
			return row["body_prediction"]
	return "(no row)"


## The largest step of watched body `k` in one frame beyond what a body moving at `speed` travels in
## that frame, over the recorded frames from `from` on (to `to`, exclusive; -1: the end).
func _jump(k: int, speed: float, from: int = 0, to: int = -1) -> float:
	var p: Array = probe.pos[k]
	var t: Array = probe.times
	var end := p.size() if to < 0 else mini(to, p.size())
	var worst := 0.0
	for i in range(maxi(1, from), end):
		var dt := float(t[i] - t[i - 1]) / 1e6
		worst = maxf(worst, (p[i] as Vector2).distance_to(p[i - 1]) - speed * dt)
	return worst


## Per frame from `from` on (to `to`, exclusive; -1: the end): the distance from watched body `k` to
## watched body `ref`, minus how far `ref` travels in `allow_s` seconds at its recorded speed (default:
## one tick); returns {excess (worst), worst, allowed (at the worst frame), mean, lag_s (mean distance
## over mean speed)}.
func _gap(k: int, ref: int, from: int = 0, to: int = -1, allow_s: float = 1.0 / 60.0) -> Dictionary:
	var worst := -INF
	var worst_err := 0.0
	var worst_allowed := 0.0
	var sum := 0.0
	var speed_sum := 0.0
	var n := 0
	var end: int = probe.pos[k].size() if to < 0 else mini(to, probe.pos[k].size())
	for i in range(from, end):
		var err := (probe.pos[k][i] as Vector2).distance_to(probe.pos[ref][i])
		var allowed: float = probe.speed[ref][i] * allow_s
		sum += err
		speed_sum += probe.speed[ref][i]
		n += 1
		if err - allowed > worst:
			worst = err - allowed
			worst_err = err
			worst_allowed = allowed
	return {"excess": worst, "worst": worst_err, "allowed": worst_allowed, "mean": sum / maxf(1.0, float(n)), "lag_s": sum / maxf(1e-6, speed_sum)}


## Per frame of [from, to) (-1: the end): how long ago watched body `ref` was where watched body `k` is
## shown, in seconds (negative: `k` is ahead), for a `ref` moving along +x; frames whose position `ref`
## did not pass within the recording are left out. Returns {mean, min, max, frames}.
func _time_lag(k: int, ref: int, from: int = 0, to: int = -1) -> Dictionary:
	var p: Array = probe.pos[ref]
	var t: Array = probe.times
	var end: int = p.size() if to < 0 else mini(to, p.size())
	var lags: Array = []
	for i in range(from, end):
		var x: float = (probe.pos[k][i] as Vector2).x
		for j in range(1, p.size()):
			var x0: float = (p[j - 1] as Vector2).x
			var x1: float = (p[j] as Vector2).x
			if x0 <= x and x <= x1 and x1 > x0:
				var tc := float(t[j - 1]) + (x - x0) / (x1 - x0) * float(t[j] - t[j - 1])
				lags.append((float(t[i]) - tc) / 1e6)
				break
	if lags.is_empty():
		return {"mean": 0.0, "min": 0.0, "max": 0.0, "frames": 0}
	return {"mean": _mean(lags), "min": lags.min(), "max": lags.max(), "frames": lags.size()}


# --- the game's freeze -------------------------------------------------------------------------------


func test_a_body_the_game_froze_waits_frozen_on_the_peer_that_simulates_it() -> void:
	timeout_sec = 15.0
	var managers := await _start_mix(1)
	var b := await _add(managers, BALL, "B", Vector2(600.0, 0.0))
	var at: Vector2 = b[0].position
	for i in 30:
		await get_tree().physics_frame
	assert_true(b[0].freeze, "the server simulates the ball, and leaves the freeze its scene set (single-player)")
	assert_eq(b[0].position, at, "so it waits in the air")
	assert_true(b[1].freeze, "the client's replica is frozen")
	b[0].launch(THROW)
	assert_true(await net.wait_until(func() -> bool: return b[0].position.x > at.x + 30.0 and b[1].position.x > at.x + 20.0), "thrown by the game, it flies, and its replica follows")
	assert_true(b[1].freeze, "the replica stays frozen")


# --- which bodies are predicted ----------------------------------------------------------------------


func test_prediction_follows_the_physics_mode_and_the_owner_without_a_jump_or_a_stale_entry() -> void:
	timeout_sec = 30.0
	var managers := await _start_mix(2)
	var a_id := net.peer_id(net.clients[0])
	var b_id := net.peer_id(net.clients[1])
	var p := await _add(managers, PUCK, "P", Vector2(-300.0, -200.0))
	var sv: RigidBody2D = p[0]
	var copies: Array = [p[1], p[2]]
	# The clients' choice, on their replicas.
	for c: Node in copies:
		Net.configure(c, {"body_prediction": "dead_reckoning"})
	assert_eq(Net.body_prediction_of(sv), NONE, "the server simulates its copy: no body prediction there")
	await wait_seconds(0.5)
	sv.launch(Vector2(PUCK_SPEED, 0.0))
	await wait_seconds(0.5)
	for i in 2:
		assert_eq(Net.body_prediction_of(copies[i]), DR, "server mode: a frozen replica on client %d" % i)
		assert_eq(_cstats(i)["predicted_bodies"], 1, "and predicted there")
		assert_eq(_cstats(i)["prediction_replicas"], 1)
	probe.watch([sv, copies[0], copies[1]])
	probe.on = true
	var marks: Array = []
	var phases: Array = []

	# simulated: every peer runs it; gdnet corrects the clients' copies and predicts nothing.
	marks.append(probe.times.size())
	phases.append("server -> simulated")
	Net.set_physics(sv, "simulated")
	assert_true(await net.wait_until(func() -> bool: return not copies[0].freeze and not copies[1].freeze), "the copies are live")
	await wait_seconds(0.5)
	for i in 2:
		assert_eq(Net.physics_of(copies[i]), "simulated")
		assert_eq(Net.body_prediction_of(copies[i]), NONE, "body_prediction is ignored for a simulated replica (client %d)" % i)
		assert_eq(_cstats(i)["predicted_bodies"], 0, "a simulated replica is never predicted")
		assert_eq(_cstats(i)["prediction_replicas"], 0, "nor kept as a frozen replica")
		assert_eq(_overlay_mode(i, copies[i]), "", "the overlay names no body_prediction")
		assert_eq(_cstats(i)["sim_bodies"], 1, "it is corrected instead")

	# Back to server: frozen and predicted again.
	marks.append(probe.times.size())
	phases.append("simulated -> server")
	Net.set_physics(sv, "server")
	assert_true(await net.wait_until(func() -> bool: return copies[0].freeze and copies[1].freeze), "the copies froze")
	await wait_seconds(0.5)
	for i in 2:
		assert_lt(_collider_gap(copies[i]), 0.5, "a copy frozen after simulating: its collider is where it is shown (client %d)" % i)
		assert_eq(Net.body_prediction_of(copies[i]), DR)
		assert_eq(_cstats(i)["predicted_bodies"], 1, "predicted again on client %d" % i)
		assert_eq(_cstats(i)["prediction_replicas"], 1)
		assert_eq(_cstats(i)["sim_bodies"], 0)

	# owner, owned by A: A simulates it and does not predict it; B predicts it.
	marks.append(probe.times.size())
	phases.append("server -> owner A")
	Net.set_physics(sv, "owner")
	assert_true(Net.set_owner(sv, a_id))
	assert_true(await net.wait_until(func() -> bool: return not copies[0].freeze and copies[1].freeze and sv.freeze), "A simulates it")
	await wait_seconds(0.5)
	assert_eq(Net.body_prediction_of(copies[0]), NONE, "never predicted on its owner")
	assert_eq(_cstats(0)["predicted_bodies"], 0)
	assert_eq(_cstats(0)["prediction_replicas"], 0, "no stale entry on the owner")
	assert_eq(Net.body_prediction_of(copies[1]), DR, "predicted on the other client")
	assert_eq(_cstats(1)["predicted_bodies"], 1)
	assert_eq(Net.body_prediction_of(sv), NONE, "the server applies the owner's states as they arrive")
	assert_lt(_collider_gap(sv), 0.5, "the server's frozen copy's collider follows the owner's states")

	# To B: A's copy freezes and is predicted, B's thaws.
	marks.append(probe.times.size())
	phases.append("owner A -> owner B")
	assert_true(Net.set_owner(sv, b_id))
	assert_true(await net.wait_until(func() -> bool: return copies[0].freeze and not copies[1].freeze), "B simulates it")
	await wait_seconds(0.5)
	probe.on = false
	assert_eq(Net.body_prediction_of(copies[0]), DR)
	assert_eq(_cstats(0)["predicted_bodies"], 1, "predicted on A now")
	assert_eq(_cstats(0)["prediction_replicas"], 1)
	assert_eq(Net.body_prediction_of(copies[1]), NONE)
	assert_eq(_cstats(1)["predicted_bodies"], 0)
	assert_eq(_cstats(1)["prediction_replicas"], 0, "no stale entry on the new owner")
	assert_gt(copies[1].linear_velocity.x, 0.9 * PUCK_SPEED, "B took over the velocity")

	# No jump: within each change's second, no frame moves a copy more than the puck travels in that
	# frame plus the bound: one tick's travel (a live copy moves once per physics tick, so one frame can
	# carry a whole tick of it) plus a first decaying step (1 - correction_decay = 15%) of the largest
	# gap a change closes: a `simulated` copy runs in the client's predicted timeline, `prediction_lead`
	# ticks ahead of the server's present a frozen replica is shown at.
	marks.append(probe.times.size())
	var lead := maxi(int(_cstats(0)["prediction_lead"]), int(_cstats(1)["prediction_lead"]))
	var jump_bound := PUCK_SPEED / 60.0 * (1.0 + 0.15 * float(lead))
	var line := ""
	var worst := 0.0
	for k in phases.size():
		var j1 := _jump(1, PUCK_SPEED, marks[k], marks[k + 1])
		var j2 := _jump(2, PUCK_SPEED, marks[k], marks[k + 1])
		worst = maxf(worst, maxf(j1, j2))
		line += "%s: A %.2f px, B %.2f px; " % [phases[k], j1, j2]
	print("mode and owner changes at %.0f px/s over a 100 ms link, largest step beyond the frame's travel: %s(bound %.2f px: a lead of %d ticks)" % [PUCK_SPEED, line, jump_bound, lead])
	assert_gt(lead, 0)
	assert_lt(worst, jump_bound, "no change moves a copy by more than %.2f px beyond its travel in one frame" % jump_bound)


# --- the mixed scene ---------------------------------------------------------------------------------


func test_a_mixed_scene_predicts_the_frozen_bodies_and_corrects_the_simulated_crate() -> void:
	timeout_sec = 30.0
	add_child(PHYSICS_FLOOR.instantiate())
	var managers := await _start_mix(2)
	var a_id := net.peer_id(net.clients[0])
	# A `server` ball thrown by the server (dead_reckoning in its scene), an `owner` vehicle client A
	# drives (dead_reckoning chosen by client B on its copy), a `simulated` crate on the floor.
	var ball := await _add(managers, BALL_RECKON, "Ball", Vector2(700.0, -200.0))
	var veh := await _add(managers, VEHICLE, "V", Vector2(600.0, 100.0), func(n: Node) -> void:
		var ne := NetEntity.new()
		ne.name = "NetEntity"
		ne.inherit_profile = true
		ne.owner_peer = a_id
		ne.physics = NetEntity.PHYSICS_OWNER
		n.add_child(ne))
	var crate := await _add(managers, CRATE, "Crate", Vector2(100.0, 285.0))
	Net.set_physics(crate[0], "simulated")
	Net.configure(veh[2], {"body_prediction": "dead_reckoning"})
	# Asking for dead reckoning on a simulated replica changes nothing.
	Net.configure(crate[2], {"body_prediction": "dead_reckoning"})
	assert_true(await net.wait_until(func() -> bool: return not crate[1].freeze and not crate[2].freeze), "the crate is live on the clients")
	assert_false(veh[1].freeze, "A drives its vehicle")
	assert_true(veh[2].freeze and veh[0].freeze, "B and the server show it")
	await wait_seconds(0.5)
	# A drives for half a second, then lets the vehicle coast; the server throws the ball as A lets go.
	probe.watch([veh[0], veh[1], veh[2]])
	probe.on = true
	Input.action_press("right")
	await wait_seconds(0.5)
	Input.action_release("right")
	var released := probe.times.size()
	ball[0].launch(THROW)
	var corrections0: int = _cstats(1)["sim_corrections"]
	await wait_seconds(0.25)
	# The server pushes the crate: the clients' live copies are corrected to its answer.
	(crate[0] as RigidBody2D).apply_central_impulse(Vector2(150.0, 0.0))
	var coasting := probe.times.size()
	await _record([ball[1], ball[2]], 0.6)
	probe.on = false
	# The ball, on both clients: gate G13.
	var ga := _g13(ball[1], 0.05)
	var gb := _g13(ball[2], 0.05)
	# The vehicle on B, coasting (its damping is in the prediction; a quarter of a second after the
	# input stopped, the correction of the unpredicted thrust has decayed), as a delay in time: how long
	# ago the server's copy (the owner's states as the server applied them), and the owner's own copy,
	# were where B shows it.
	var vs := _time_lag(2, 0, coasting)
	var vo := _time_lag(2, 1, coasting)
	# Driving, for the record: dead reckoning does not know the thrust.
	var ds := _time_lag(2, 0, 30, released)
	var dsp: float = probe.speed[1][released - 1]
	var csp: float = probe.speed[1][coasting]
	var st_a := _cstats(0)
	var st_b := _cstats(1)
	var corrections: int = int(st_b["sim_corrections"]) - corrections0
	print("mixed scene, 100 ms link: ball (server, dead reckoning) worst %.2f px on A, %.2f px on B against one tick's travel %.2f px, mean %.2f / %.2f px; vehicle (owner A, dead reckoning on B), coasting from %.0f px/s over %d frames: behind the server's copy by %.1f ms on average (%.1f..%.1f), behind the owner by %.1f ms (%.1f..%.1f); driving at up to %.0f px/s: behind the server's copy by %.1f ms (%.1f..%.1f); crate (simulated): %d corrections on B, predicted bodies A %d B %d, sim bodies A %d B %d, horizon on B %.1f ms" % [ga["worst"], gb["worst"], gb["travel"], ga["mean"], gb["mean"], csp, vs["frames"], vs["mean"] * 1000.0, vs["min"] * 1000.0, vs["max"] * 1000.0, vo["mean"] * 1000.0, vo["min"] * 1000.0, vo["max"] * 1000.0, dsp, ds["mean"] * 1000.0, ds["min"] * 1000.0, ds["max"] * 1000.0, corrections, st_a["predicted_bodies"], st_b["predicted_bodies"], st_a["sim_bodies"], st_b["sim_bodies"], st_b["prediction_horizon_ms"]])
	assert_gt(ga["frames"], 20, "frames were recorded")
	assert_lt(ga["excess"], 0.05, "G13 on A: the ball is within one tick's travel of the server's present")
	assert_lt(gb["excess"], 0.05, "G13 on B too, beside an owner body and a simulated crate")
	assert_gt(vs["frames"], 20, "coasting frames were measured")
	assert_lt(absf(vs["mean"]), 1.0 / 60.0, "the coasting vehicle on B is on average within one tick of the server's copy")
	assert_lt(maxf(vs["max"], -vs["min"]), SERVER_COPY_BOUND_S, "and on every frame within %.0f ms of it" % (SERVER_COPY_BOUND_S * 1000.0))
	assert_lt(vo["max"], OWNER_BOUND_S, "behind the owner's present by at most %.0f ms" % (OWNER_BOUND_S * 1000.0))
	assert_gt(vo["min"], 0.0, "and never ahead of it")
	assert_eq(st_a["predicted_bodies"], 1, "A predicts the ball only (the vehicle is its own, the crate simulated)")
	assert_eq(st_b["predicted_bodies"], 2, "B predicts the ball and the vehicle, not the crate")
	assert_eq(st_b["sim_bodies"], 1, "B corrects the crate")
	assert_gt(corrections, 0, "the server's push corrected B's crate")
	assert_eq(Net.body_prediction_of(crate[2]), NONE, "dead_reckoning asked for a simulated crate is ignored")
	assert_eq(Net.body_prediction_of(veh[1]), NONE, "and an owner body on its owner")
	assert_eq(_overlay_mode(1, crate[2]), "")
	assert_true(await net.wait_until(func() -> bool: return (crate[2] as Node2D).position.distance_to(crate[0].position) < 1.0, 6.0), "B's crate ends where the server's is")


## The server's copy of an `owner` body changes when the owner's states arrive, not on its own ticks:
## a server tick may apply two of them or none, so on top of gate G13's one tick the relayed stream
## adds one tick of arrival jitter: B's prediction is within two ticks of the server's copy.
const SERVER_COPY_BOUND_S := 2.0 / 60.0
## The owner's states reach the other client through the server: the server applies them as they
## arrive, half A's round trip (50 ms here) after A simulated them, so B's prediction to the server's
## present trails the owner's own present by that much, plus up to a tick at the owner, one at the
## server and one of prediction error: 50 ms plus three ticks.
const OWNER_BOUND_S := 0.05 + 3.0 / 60.0


# --- the shared correction settings -------------------------------------------------------------------


func test_the_shared_correction_settings_are_metres_in_2d_and_3d() -> void:
	timeout_sec = 20.0
	var managers := await _start_mix(1)
	var p := await _add(managers, BALL, "P", Vector2(600.0, -200.0))
	Net.configure(p[1], {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.3)
	p[0].launch(Vector2(300.0, -600.0))
	await wait_seconds(0.2)
	# 2D reads metres at 100 px per metre: the default 0.02 m is 2 px, above the 0.1 px steps a thrown
	# ball's snapshots disagree with its prediction by; 0.0001 m (0.01 px) counts them.
	var c0: int = _cstats(0)["prediction_corrections"]
	await wait_seconds(0.5)
	var c_default: int = int(_cstats(0)["prediction_corrections"]) - c0
	ProjectSettings.set_setting("gdnet/physics/correction_position", 0.0001)
	var c1: int = _cstats(0)["prediction_corrections"]
	await wait_seconds(0.5)
	var c_tiny: int = int(_cstats(0)["prediction_corrections"]) - c1
	ProjectSettings.set_setting("gdnet/physics/correction_position", null)
	net.teardown()
	net = null
	await wait_frames(2)
	# 3D reads the same metres as metres: 0.0001 m (0.1 mm) counts a thrown ball's 1 mm steps.
	net = NetFixture.new(self, 1)
	net.set_emulation(net.clients[0], _link(50.0))
	net.start()
	assert_true(await net.wait_connected(10.0))
	var m3 := net.add_managers()
	(m3["ClientA"] as Node).child_entered_tree.connect(func(n: Node) -> void:
		if n is RigidBody3D:
			(n as RigidBody3D).collision_layer = CLIENT_LAYER
			(n as RigidBody3D).collision_mask = 0)
	var b: RigidBody3D = BALL_3D.instantiate()
	b.name = "B3"
	(m3["Server"] as Node).add_child(b)
	assert_true(await net.wait_until(func() -> bool: return net.clients[0].has_node("World/B3"), 10.0))
	var r: Node = net.clients[0].get_node("World/B3")
	Net.configure(r, {"body_prediction": "dead_reckoning"})
	await wait_seconds(0.3)
	b.launch(Vector3(3.0, 6.0, 0.0))
	await wait_seconds(0.3)
	var d0: int = _cstats(0)["prediction_corrections"]
	await wait_seconds(0.4)
	var d_default: int = int(_cstats(0)["prediction_corrections"]) - d0
	ProjectSettings.set_setting("gdnet/physics/correction_position", 0.0001)
	var d1: int = _cstats(0)["prediction_corrections"]
	await wait_seconds(0.4)
	var d_tiny: int = int(_cstats(0)["prediction_corrections"]) - d1
	print("correction_position: 2D ball in flight, corrections in 0.5 s: %d at the default 0.02 m, %d at 0.0001 m; 3D ball in flight, in 0.4 s: %d at the default, %d at 0.0001 m" % [c_default, c_tiny, d_default, d_tiny])
	assert_eq(c_default, 0, "2D: the default 0.02 m reads as 2 px")
	assert_gt(c_tiny, 0, "2D: 0.0001 m reads as 0.01 px")
	assert_eq(d_default, 0, "3D: the default reads as 2 cm")
	assert_gt(d_tiny, 0, "3D: 0.0001 m reads as 0.1 mm, not 1 cm")
