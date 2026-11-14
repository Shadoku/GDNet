extends TestCase
## Test fixture (WP19 body prediction, 2D): the fixture, spawning and measuring helpers the
## body prediction test files share. The link is 50 ms each way (a 100 ms round trip) unless a test
## says otherwise. Positions are sampled in the bodies' own `_process` (gdnet writes the replicas'
## poses in the session's `_process`, first); the server's copy is sampled in the same frame, which
## is the server's present. The branches share one physics space, so the clients' copies of bodies
## are moved to a collision layer of their own as they enter the manager.

const BALL := preload("res://tests/fixtures/predict/ball.tscn")
const BALL_RECKON := preload("res://tests/fixtures/predict/ball_reckon.tscn")
const THRUST := preload("res://tests/fixtures/predict/thrust_ball.tscn")
const THRUST_INTERP := preload("res://tests/fixtures/predict/thrust_ball_interp.tscn")
const ODD := preload("res://tests/fixtures/predict/odd_ball.tscn")
const BOUNCY := preload("res://tests/fixtures/predict/bouncy_ball.tscn")
const PUCK := preload("res://tests/fixtures/predict/puck.tscn")
const FLOOR := preload("res://tests/fixtures/predict/floor.tscn")
const WALKER := preload("res://tests/fixtures/predict/walker.tscn")
const SETTINGS: Array[String] = [
	"gdnet/physics/body_prediction",
	"gdnet/physics/predict_max_ms",
	"gdnet/physics/correction_position",
	"gdnet/physics/correction_angle",
	"gdnet/physics/correction_decay",
]
## The client's copies of bodies collide on this layer only.
const CLIENT_LAYER := 1 << 4
## Half a 2D `mm` step (0.1 px) on two axes: the most a replica at rest differs from the server.
const HALF_STEP := 0.071
## A thrown ball: 300 px/s to the right, 400 px/s up; gravity (980 px/s^2) brings it down.
const THROW := Vector2(300.0, -400.0)

var net: NetFixture
var _fps: int = 0


func before_each() -> void:
	_fps = Engine.max_fps


func after_each() -> void:
	Input.action_release("right")
	if net != null:
		net.teardown()
		net = null
	Engine.max_fps = _fps
	for s in SETTINGS:
		if ProjectSettings.has_setting(s):
			ProjectSettings.set_setting(s, null)


func _link(ms: float) -> NetEmulation:
	var e := NetEmulation.new()
	e.send_latency_ms = ms
	e.receive_latency_ms = ms
	e.seed = 19
	return e


## A fixture with one client over a `ms` each-way link; managers under `World`; the client's bodies on a
## layer of their own (walkers collide with the client's bodies only).
func _start(ms: float = 50.0, mode: NetFixture.HostMode = NetFixture.HostMode.DEDICATED) -> Dictionary:
	net = NetFixture.new(self, 1, mode)
	for c in net.clients:
		net.set_emulation(c, _link(ms))
	net.start()
	assert_true(await net.wait_connected(10.0))
	var managers := net.add_managers()
	for c in net.clients:
		(managers[String(c.name)] as Node).child_entered_tree.connect(func(n: Node) -> void:
			if n is RigidBody2D:
				(n as RigidBody2D).collision_layer = CLIENT_LAYER
				(n as RigidBody2D).collision_mask = 0
			elif n is CharacterBody2D:
				(n as CharacterBody2D).collision_mask = CLIENT_LAYER)
	return managers


## Adds `scene` at `at` under the server's manager; returns [server copy, client copy].
func _spawn(managers: Dictionary, scene: PackedScene, node_name: String, at: Vector2) -> Array:
	var n: Node2D = scene.instantiate()
	n.name = node_name
	n.position = at
	(managers["Server"] as Node).add_child(n)
	var c: Node = net.clients[0]
	var path := "World/" + node_name
	assert_true(await net.wait_until(func() -> bool: return c.has_node(path), 10.0), "the copy reached the client")
	var r: Node2D = c.get_node(path)
	r.set("other", n)
	return [n, r]


func _floor(y: float) -> void:
	var f: Node2D = FLOOR.instantiate()
	f.position = Vector2(0.0, y + 20.0)
	net.server.add_child(f)


func _stats() -> Dictionary:
	return net.client_sessions[0].stats()


## Per recorded frame of a replica: (distance to the server's present position, the server's travel in
## one tick at that moment).
func _errors(r: Node) -> Array:
	var out: Array = []
	var shown: PackedVector2Array = r.get("shown")
	var other: PackedVector2Array = r.get("other_shown")
	var speed: PackedFloat64Array = r.get("other_speed")
	for i in shown.size():
		out.append(Vector2(shown[i].distance_to(other[i]), speed[i] / Engine.physics_ticks_per_second))
	return out


func _mean(a: Array) -> float:
	var s := 0.0
	for x: float in a:
		s += x
	return s / maxf(1.0, float(a.size()))


func _record(bodies: Array, seconds: float) -> void:
	for b: Node in bodies:
		b.call("clear_recording")
		b.set("recording", true)
	await wait_seconds(seconds)
	for b: Node in bodies:
		b.set("recording", false)


## Gate G13: the worst frame's error minus one tick's travel (<= 0 passes), the mean error, the mean
## trail in units of `velocity x one-way delay`.
func _g13(r: Node, one_way_s: float) -> Dictionary:
	var worst := -INF
	var worst_err := 0.0
	var worst_travel := 0.0
	var errs: Array = []
	var trails: Array = []
	for e: Vector2 in _errors(r):
		errs.append(e.x)
		trails.append(e.x / maxf(1e-6, e.y * Engine.physics_ticks_per_second * one_way_s))
		if e.x - e.y > worst:
			worst = e.x - e.y
			worst_err = e.x
			worst_travel = e.y
	# The signed error along the server's motion, in ticks of travel (positive: ahead of the server).
	var leads: Array = []
	var shown: PackedVector2Array = r.get("shown")
	var other: PackedVector2Array = r.get("other_shown")
	var speed: PackedFloat64Array = r.get("other_speed")
	for i in range(1, other.size() - 1):
		var dir := (other[i + 1] - other[i - 1]).normalized()
		if dir != Vector2.ZERO and speed[i] > 0.0:
			leads.append((shown[i] - other[i]).dot(dir) / (speed[i] / Engine.physics_ticks_per_second))
	return {"excess": worst, "worst": worst_err, "travel": worst_travel, "mean": _mean(errs), "trail": _mean(trails), "frames": errs.size(), "lead": _mean(leads), "lead_min": leads.min() if not leads.is_empty() else 0.0, "lead_max": leads.max() if not leads.is_empty() else 0.0}
