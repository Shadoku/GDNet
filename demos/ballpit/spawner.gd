class_name BallpitSpawner
extends Node2D
## The pit and its Spawner. On the server it fills the NetManager with balls and gives every peer a
## paddle; every other peer just receives them. Nothing here is network code beyond `Net.spawn_for`.

const BALL := preload("res://demos/ballpit/ball.tscn")
const PADDLE := preload("res://demos/ballpit/paddle.tscn")
const TRUSTED_PADDLE := preload("res://demos/ballpit/trusted_paddle.tscn")
const SIZE := Vector2(640, 400)

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
var _rng := RandomNumberGenerator.new()

@onready var manager: NetManager = $NetManager


func _ready() -> void:
	_rng.seed = 7
	_shift($Walls)
	manager.child_entered_tree.connect(_shift)


## Called once the session runs. Hash telemetry samples two bodies a tick: plenty for a check, a
## small fraction of the budget.
func start() -> void:
	var session: NetSession = Net.session_for(self)
	session.hash_sample = 2
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


func _shift(node: Node) -> void:
	if node is CollisionObject2D and layer_shift > 0:
		var body := node as CollisionObject2D
		body.collision_layer <<= layer_shift
		body.collision_mask <<= layer_shift
