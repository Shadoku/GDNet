class_name BallpitSpawner
extends Node2D
## The pit and its Spawner. On the server it fills the NetManager with balls and gives every peer a
## paddle; every other peer just receives them. Beyond `Net.spawn_for`, the network code is the two
## switches at the end: who simulates the balls, and how this screen shows them.

const BALL := preload("res://demos/ballpit/ball.tscn")
const PADDLE := preload("res://demos/ballpit/paddle.tscn")
const TRUSTED_PADDLE := preload("res://demos/ballpit/trusted_paddle.tscn")
const SIZE := Vector2(640, 400)
## The balls' physics modes the HUD offers (docs/guide/10_physics_and_bandwidth.md). `owner` is left
## out: no player owns a ball.
const PHYSICS := {"server": NetEntity.PHYSICS_SERVER, "handoff": NetEntity.PHYSICS_HANDOFF, "simulated": NetEntity.PHYSICS_SIMULATED}
## How a client shows the balls it does not simulate.
const PREDICTION := {"interpolate": NetEntity.BODY_PREDICTION_INTERPOLATE, "dead_reckoning": NetEntity.BODY_PREDICTION_DEAD_RECKONING}

## How many balls the server keeps in the pit.
@export var ball_count: int = 300: set = set_ball_count
## True on a dedicated server: peer 1 gets no paddle.
@export var dedicated: bool = false
## Moves every collision layer and mask of the pit's bodies up by this many bits. A game leaves it at
## 0; the tests hold several peers in one process, one physics space, and give each its own layers.
@export var layer_shift: int = 0

var _balls: Array[Node] = []
var _paddles: Dictionary = {}
var _trusted: bool = false
var _physics: String = "server"
var _rng := RandomNumberGenerator.new()

@onready var manager: NetManager = $NetManager


func _ready() -> void:
	_rng.seed = 7
	_shift($Walls)
	manager.child_entered_tree.connect(_shift)
	manager.child_entered_tree.connect(_fit_paddle)


## Called once the session runs. Hash telemetry samples two bodies a tick: plenty for a check, a
## small fraction of the budget.
func start() -> void:
	var session: NetSession = Net.session_for(self)
	session.hash_sample = 2
	session.state.changed.connect(func(path: String, value: Variant) -> void:
		if path == "ballpit/physics":
			_fit_paddles(value))
	_fit_paddles(session.state.get("ballpit/physics", "server"))
	if not session.is_server():
		return
	session.peer_joined.connect(spawn_paddle)
	session.peer_left.connect(remove_paddle)
	if not dedicated:
		spawn_paddle(1)
	for id in session.peers():
		spawn_paddle(id)
	set_ball_count(ball_count)


## Server: adds or frees balls until `n` are in the pit. New balls start in rows from the floor up,
## a little apart, so they settle without bursting.
func set_ball_count(n: int) -> void:
	ball_count = maxi(n, 0)
	var session: NetSession = Net.session_for(self) if is_node_ready() else null
	if session == null or not session.is_server():
		return
	while _balls.size() > ball_count:
		_balls.pop_back().queue_free()
	while _balls.size() < ball_count:
		var i := _balls.size()
		var ball: RigidBody2D = BALL.instantiate()
		ball.name = "Ball%d" % i
		var row := i / 30
		ball.position = Vector2(12.0 + (i % 30) * 20.5, SIZE.y - 10.0 - (row % 20) * 18.0) \
			+ Vector2(_rng.randf_range(-2.0, 2.0), _rng.randf_range(-2.0, 2.0))
		manager.add_child(ball)
		_balls.append(ball)


## Server: a paddle for peer `id`, owned and predicted by that peer.
func spawn_paddle(id: int) -> void:
	var scene := TRUSTED_PADDLE if _trusted else PADDLE
	var paddle: Node2D = Net.spawn_for(id, scene, manager, "%s%d" % [scene.resource_path.get_file().get_basename(), id])
	paddle.position = Vector2(80.0 + 120.0 * (_paddles.size() % 5), 60.0)
	_paddles[id] = paddle


func remove_paddle(id: int) -> void:
	if is_instance_valid(_paddles.get(id)):
		_paddles[id].queue_free()
	_paddles.erase(id)


## Server: spawns every paddle again, as a `client_owned` (trusted) one or as a predicted one.
func set_trusted_paddles(on: bool) -> void:
	_trusted = on
	Net.show_overlay(on)
	for id: int in _paddles.keys():
		remove_paddle(id)
		spawn_paddle(id)


## Server: kicks every ball upwards.
func shake() -> void:
	for ball: RigidBody2D in _balls:
		ball.apply_central_impulse(Vector2(_rng.randf_range(-150.0, 150.0), _rng.randf_range(-450.0, -250.0)))


## Server: who simulates the balls, `"server"`, `"handoff"` or `"simulated"`. The manager's default
## reaches every ball (those spawned later too) and every peer at the next tick; the keyed state tells
## every peer's Spawner, which fits its paddles to the mode.
func set_ball_physics(mode: String) -> void:
	var session: NetSession = Net.session_for(self)
	if not session.is_server():
		return
	manager.physics = PHYSICS[mode]
	session.state.set("ballpit/physics", mode)
	_fit_paddles(mode)


## The balls' physics mode as this peer last heard it.
func ball_physics() -> String:
	return _physics


## This peer's screen only: `"interpolate"` shows the balls a little in the past, exactly;
## `"dead_reckoning"` carries each ball's newest state forward to the present. It changes nothing on
## the wire or for the other peers, and applies only to the balls this peer shows frozen.
func set_ball_prediction(mode: String) -> void:
	manager.body_prediction = PREDICTION[mode]


## With `server` balls a paddle collides with the walls only: the balls a client sees are frozen
## pictures of the past, and a predicted paddle bumping into them would stop where the server's did
## not. With `handoff` and `simulated` balls the ones a paddle touches are live, so it collides with
## the balls too (and, in `handoff`, takes them over as it pushes them).
func _fit_paddles(mode: String) -> void:
	_physics = mode
	for node in manager.get_children():
		_fit_paddle(node)


func _fit_paddle(node: Node) -> void:
	if node is CharacterBody2D:
		var mask := 1 if _physics == "server" else 3
		(node as CharacterBody2D).collision_mask = mask << layer_shift


func _shift(node: Node) -> void:
	if node is CollisionObject2D and layer_shift > 0:
		var body := node as CollisionObject2D
		body.collision_layer <<= layer_shift
		body.collision_mask <<= layer_shift
