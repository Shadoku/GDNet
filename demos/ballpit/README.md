# Ballpit: hundreds of colliding bodies

The second of gdnet's two instructional demos (docs/PLAN_NETCODE.md 2.11, WP14). A box full of balls, a
paddle per player, sliders for the number of balls and the bandwidth budget. The balls are plain
`RigidBody2D` scenes with no script: the server simulates them, every client receives their position,
rotation and velocities and shows them interpolated. Read it top to bottom: every file of the demo is
listed here, in full (a test, `tests/integration/test_ballpit_demo.gd`, checks that these listings are the
files). The last chapter is about bandwidth: priority, the budget, degradation and what the meter shows.

The concepts are in `docs/guide/10_physics_and_bandwidth.md`; this page is the worked example.

## Running it

* **In the editor**: open `demos/ballpit/main.tscn` and press *Run Current Scene* (F6). Choose **Offline**
  to play alone (the host is a client of itself), **Host** to host on port 7351, **Join** to join the
  address in the box.
* **Two clients on one machine**: *Debug > Customize Run Instances...*, enable *Enable Multiple
  Instances*, set the count to 3, and give the first instance the launch argument `-- --server`. Run
  `main.tscn`; press **Join** in the two other windows.
* **A headless server**: `godot --headless --path . demos/ballpit/main.tscn -- --server`.
* **The tests**: `python3 tools/check.py --only integration --filter ballpit` runs the demo headless: a server and
  a client in one process, 300 balls.

Move your paddle with the arrow keys or WASD. Push it into the pile and watch the balls scatter.

## The title screen: `main.tscn` and `main.gd`

`main.tscn` holds the menu, the pit and the controls. Nothing in it is networked; the pit's `NetManager`
is what makes the balls and paddles entities.

```text file=demos/ballpit/main.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/ballpit/main.gd" id="1_main"]
[ext_resource type="PackedScene" path="res://demos/ballpit/pit.tscn" id="2_pit"]
[ext_resource type="PackedScene" path="res://demos/ballpit/hud.tscn" id="3_hud"]

[node name="Main" type="Control"]
layout_mode = 3
anchors_preset = 15
anchor_right = 1.0
anchor_bottom = 1.0
script = ExtResource("1_main")

[node name="Pit" parent="." instance=ExtResource("2_pit")]
position = Vector2(400, 160)

[node name="Menu" type="VBoxContainer" parent="."]
layout_mode = 1
anchors_preset = 8
anchor_left = 0.5
anchor_top = 0.5
anchor_right = 0.5
anchor_bottom = 0.5
offset_left = -100.0
offset_top = -90.0
offset_right = 100.0
offset_bottom = 90.0

[node name="Title" type="Label" parent="Menu"]
layout_mode = 2
text = "Ballpit"
horizontal_alignment = 1

[node name="Host" type="Button" parent="Menu"]
layout_mode = 2
text = "Host"

[node name="Address" type="LineEdit" parent="Menu"]
layout_mode = 2
text = "127.0.0.1"

[node name="Join" type="Button" parent="Menu"]
layout_mode = 2
text = "Join"

[node name="Offline" type="Button" parent="Menu"]
layout_mode = 2
text = "Offline"

[node name="Hud" parent="." node_paths=PackedStringArray("pit") instance=ExtResource("3_hud")]
visible = false
pit = NodePath("../Pit")
```

`main.gd` is the Doors demo's title screen with another port: start a session, then tell the pit and the
controls that it runs.

```gdscript file=demos/ballpit/main.gd
extends Control
## The title screen: Host, Join or Offline. `--server` after `--` on the command line starts a
## dedicated server: `godot --headless --path . demos/ballpit/main.tscn -- --server`.

const PORT := 7351

@onready var pit: BallpitSpawner = $Pit
@onready var menu: Control = $Menu
@onready var hud: BallpitHud = $Hud


func _ready() -> void:
	$Menu/Host.pressed.connect(func() -> void: _start(Net.host(PORT)))
	$Menu/Join.pressed.connect(func() -> void: _start(Net.join($Menu/Address.text, PORT)))
	$Menu/Offline.pressed.connect(func() -> void: _start(Net.offline()))
	if "--server" in OS.get_cmdline_user_args():
		pit.dedicated = true
		_start(Net.host(PORT))


func _start(error: Error) -> void:
	if error != OK:
		push_error("could not start the session: %s" % error_string(error))
		return
	menu.hide()
	hud.show()
	pit.start()
	hud.start()
```

## The pit: `pit.tscn` and `spawner.gd`

The pit is a box of four `StaticBody2D` walls, an outline, and a `NetManager`. The walls sit beside the
manager, not under it: they are level geometry, identical on every peer, and never change. Everything the
server adds under the manager at run time becomes an entity: the balls and the paddles.

```text file=demos/ballpit/pit.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/ballpit/spawner.gd" id="1_spawner"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_floor"]
size = Vector2(720, 40)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_side"]
size = Vector2(40, 400)

[node name="Pit" type="Node2D"]
script = ExtResource("1_spawner")

[node name="Walls" type="StaticBody2D" parent="."]
collision_mask = 0

[node name="Floor" type="CollisionShape2D" parent="Walls"]
position = Vector2(320, 420)
shape = SubResource("RectangleShape2D_floor")

[node name="Ceiling" type="CollisionShape2D" parent="Walls"]
position = Vector2(320, -20)
shape = SubResource("RectangleShape2D_floor")

[node name="Left" type="CollisionShape2D" parent="Walls"]
position = Vector2(-20, 200)
shape = SubResource("RectangleShape2D_side")

[node name="Right" type="CollisionShape2D" parent="Walls"]
position = Vector2(660, 200)
shape = SubResource("RectangleShape2D_side")

[node name="Outline" type="Line2D" parent="."]
points = PackedVector2Array(0, 0, 640, 0, 640, 400, 0, 400)
closed = true
width = 2.0

[node name="NetManager" type="NetManager" parent="."]
interaction_radius = 50.0
```

The manager's `interaction_radius = 50.0` overrides the project setting
`gdnet/budget/interaction_radius` (300 px) for this pit: a ball within 50 px of a player's paddle counts as
*interacting* with that player and is sent to it first (see "Bandwidth" below). 50 px is about two balls
around the paddle; a radius much larger than that would mark more balls than a small budget can update.

`spawner.gd` is the Spawner. On the server it spawns the balls (in loose rows from the floor up, so they
settle without bursting) and gives every peer a paddle with `Net.spawn_for`. On a client it does nothing:
the balls and paddles arrive as entities. It also sets `hash_sample`, the number of entities hash
telemetry checks per tick (docs/guide/08_diagnostics.md): two bodies a tick are enough to catch a
divergence and cost well under 1 % of the budget.

```gdscript file=demos/ballpit/spawner.gd
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
```

`layer_shift` exists for the tests: they hold a server and a client in one process, which share one
physics space, so the client's copy of the pit moves its bodies to collision layers of their own. A game
leaves it at 0.

## A ball: `ball.tscn`

A `RigidBody2D` with a circle and a polygon. No script, no `NetEntity`: rung 0 of the ladder
(docs/PLAN_NETCODE.md section 1). gdnet infers its profile: `position`, `rotation`, `scale`,
`linear_velocity` and `angular_velocity` are hot (sent when they change, quantised, interpolated), the
`visible`, `modulate` and `z_index` of the body and its parts are cold.

```text file=demos/ballpit/ball.tscn
[gd_scene format=3]

[sub_resource type="CircleShape2D" id="CircleShape2D_ball"]
radius = 8.0

[node name="Ball" type="RigidBody2D"]
collision_layer = 2
collision_mask = 7
angular_damp = 3.0

[node name="Shape" type="CollisionShape2D" parent="."]
shape = SubResource("CircleShape2D_ball")

[node name="Body" type="Polygon2D" parent="."]
color = Color(1, 0.55, 0.25, 1)
polygon = PackedVector2Array(0, -8, 5.7, -5.7, 8, 0, 5.7, 5.7, 0, 8, -5.7, 5.7, -8, 0, -5.7, -5.7)
```

On a client every ball is a *replica*: gdnet sets `freeze = true` and turns its physics processing off, so
the client never simulates it; the snapshots move it. `angular_damp` makes a ball that rolls along the
floor come to rest, so a pile falls asleep instead of creeping for seconds.

A sleeping body does not move: its position, rotation and velocities stay exactly the same, so the server
records no change and sends nothing for it. A settled pit of 300 balls costs nothing but the pings (the
test measures 56 B/s in total, every channel included).

## The paddle: `paddle.tscn`, `paddle.gd` and `trusted_paddle.tscn`

The paddle is rung 1's predicted player: a `CharacterBody2D` whose `_physics_process` reads
`Net.input(self)` instead of `Input`. Its owner moves it on the very tick the key goes down; the server
runs the same code with the owner's input.

```text file=demos/ballpit/paddle.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/ballpit/paddle.gd" id="1_paddle"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_paddle"]
size = Vector2(64, 16)

[node name="Paddle" type="CharacterBody2D"]
collision_layer = 4
collision_mask = 1
script = ExtResource("1_paddle")

[node name="Shape" type="CollisionShape2D" parent="."]
shape = SubResource("RectangleShape2D_paddle")

[node name="Body" type="Polygon2D" parent="."]
color = Color(0.3, 0.7, 1, 1)
polygon = PackedVector2Array(-32, -8, 32, -8, 32, 8, -32, 8)
```

```gdscript file=demos/ballpit/paddle.gd
class_name BallpitPaddle
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1). Its collision mask holds only the
## walls: the paddle's own motion never depends on the balls, so the owner's prediction is exact,
## while the balls (which do collide with it) are pushed by the server's copy.

const SPEED := 300.0


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
```

Look at the collision masks. The paddle (layer 3, value 4) collides only with the walls (mask 1); the balls
(layer 2) collide with walls, balls and paddles (mask 7). So the balls bounce off the paddle, but the
paddle's own movement never depends on where the balls are. That is what makes the prediction exact: on
the owner the balls are replicas a few ticks in the past, and a paddle that bumped into them would stop
where the server's paddle did not, then roll back. With this mask the owner's paddle and the server's
always agree (the test asserts zero rollbacks during a push).

The price is honest and worth knowing: **the owner sees its push one round trip late.** The server's
paddle pushes the server's balls; the owner sees them move when the next snapshots arrive, plus the jitter
buffer. Over a 50 ms link (each way) the test measures 5 or 6 ticks from the server's push to the first
ball moving on the owner's screen, on top of the input's own trip to the server (the paddle on the owner's
screen is already in the pile by then: contact on the owner is visual only). Until WP15 (cooperative physics ownership) this
is the baseline for any server-owned body a client touches.

`trusted_paddle.tscn` is the same scene with a `NetEntity` that sets `client_owned = true`:

```text file=demos/ballpit/trusted_paddle.tscn
[gd_scene format=3]

[ext_resource type="PackedScene" path="res://demos/ballpit/paddle.tscn" id="1_paddle"]

[node name="Paddle" instance=ExtResource("1_paddle")]

[node name="NetEntity" type="NetEntity" parent="."]
inherit_profile = true
client_owned = true
```

A `client_owned` entity is simulated by its owner only and the server accepts the owner's state as it is
(docs/guide/07_authority.md). There is no prediction and no rollback: the paddle is exactly where the
owner puts it. The trade-off is trust: a modified client can put its paddle anywhere, through walls,
across the pit, and the server will relay it to everyone. The HUD's **Trusted paddles** toggle spawns
every paddle again from this scene and shows the ownership overlay, where trusted entities are amber.
Use it for prototypes and cooperative games among friends; keep the predicted paddle where cheating
matters. The balls stay server-owned either way.

## The controls: `hud.tscn` and `hud.gd`

```text file=demos/ballpit/hud.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/ballpit/hud.gd" id="1_hud"]

[node name="Hud" type="VBoxContainer"]
offset_left = 8.0
offset_top = 8.0
offset_right = 248.0
offset_bottom = 248.0
script = ExtResource("1_hud")

[node name="BallsLabel" type="Label" parent="."]
layout_mode = 2
text = "Balls"

[node name="Balls" type="HSlider" parent="."]
layout_mode = 2
max_value = 600.0
step = 10.0
value = 300.0

[node name="BudgetLabel" type="Label" parent="."]
layout_mode = 2
text = "Budget"

[node name="Budget" type="HSlider" parent="."]
layout_mode = 2
min_value = 2048.0
max_value = 65536.0
step = 2048.0
value = 32768.0

[node name="Trusted" type="CheckBox" parent="."]
layout_mode = 2
text = "Trusted paddles (client_owned)"

[node name="Shake" type="Button" parent="."]
layout_mode = 2
text = "Shake"

[node name="Meter" type="CheckBox" parent="."]
layout_mode = 2
button_pressed = true
text = "Meter (F9)"
```

```gdscript file=demos/ballpit/hud.gd
class_name BallpitHud
extends VBoxContainer
## The controls: ball count, the snapshot budget, trusted paddles, a shake, the meter. They change the
## server's world, so only the host can use them; a client sees them greyed out.

## The pit the controls act on.
@export var pit: BallpitSpawner


func _ready() -> void:
	$Balls.value_changed.connect(func(v: float) -> void:
		pit.ball_count = int(v)
		_show_values())
	$Budget.value_changed.connect(func(v: float) -> void:
		Net.budget_bytes_per_s = int(v)
		_show_values())
	$Trusted.toggled.connect(pit.set_trusted_paddles)
	$Shake.pressed.connect(pit.shake)
	$Meter.toggled.connect(Net.show_meter)


## Called once the session runs.
func start() -> void:
	var session: NetSession = Net.session_for(pit)
	var host := session.is_server()
	$Balls.editable = host
	$Budget.editable = host
	$Trusted.disabled = not host
	$Shake.disabled = not host
	$Balls.set_value_no_signal(pit.ball_count)
	$Budget.set_value_no_signal(Net.budget_bytes_per_s)
	Net.show_meter($Meter.button_pressed)
	_show_values()


func _show_values() -> void:
	$BallsLabel.text = "Balls: %d" % pit.ball_count
	$BudgetLabel.text = "Budget: %d B/s per peer" % int($Budget.value)
```

The budget slider sets `Net.budget_bytes_per_s`: snapshot bytes per second per peer, applied to the
running session at its next tick.

## Bandwidth

Three hundred moving balls do not fit in any sensible budget: each update of a ball is about 16 bytes
(4 changed fields, quantised, plus the entry's header), so updating all of them every tick at 60 Hz would
take 280 KB/s per client. gdnet's default budget is 32 KB/s. What decides which balls go out?

**Most-recent-state.** A snapshot carries only fields that changed since the client last acknowledged
them, and a lost snapshot is never resent: its fields are simply still pending, and the next snapshot
carries their current values. A ball that is not sent this tick is not lost; it is late.

**Priority.** Every tick, each entity with something pending adds a weight to its accumulator for that
client: 1,000,000 when the client owns it (its paddle), 100 when it *interacts* with the client, 1
otherwise. The snapshot is filled in accumulator order and a sent entity drops back to zero. So the
paddle goes out every tick, interacting balls nearly every tick, and the others queue up: a ball that waited
k ticks has weight k, and it beats an interacting ball only after waiting 100 ticks.

**Interaction.** Before the snapshots are built, the server marks, for each client with a player, the
entities within the manager's `interaction_radius` of the player and the bodies touching the player (a
`RigidBody` with `contact_monitor` on reports its colliding bodies; a `CharacterBody`, its last
`move_and_slide` collisions). In this demo that is every ball within 50 px of a paddle. The pass takes
about 0.4 ms for 300 bodies on the test machine (`interaction_ms` in `stats()`).

**The budget.** A token bucket per client fills at `budget_bytes_per_s` and the snapshot never exceeds it,
nor the 1,200-byte MTU (the bucket holds at most two MTUs of slack). With the budget halved to 16 KB/s
the test measures 16,379 B/s sent over a one-second window (one snapshot of 273 bytes a tick, about 17
ball updates), the balls near the paddle updated on every tick they moved, and the far moving balls on 5 %
of theirs: they *degrade*, they are not dropped, and the budget is not exceeded by more than the slack.

**The meter** (F9, on at start) shows it all. The `budget` line reads, for example,
`budget 100% of 16384 B/s per peer  deferred: 17205 ent/s  interacting 4`: the share of the budget the
snapshots use, how many entities with pending changes were left out of snapshots per second (the
degradation counter: 0 while everything fits, and it rises as soon as the budget is the limit), and how
many entities are marked interacting. On a client the `in` line's `ch1` is the snapshot bytes arriving.
Move the budget slider and watch: below about 20 KB/s the far balls visibly stutter while those around
your paddle stay smooth; at 64 KB/s and 300 balls still nothing is dropped, only deferred less.

`stats()` has the same figures: `deferred_entities`, `deferred_fields` (totals), their `_per_s` and
`_tick` forms, `interacting`, `interaction_ms`, and `NetSession.interacting(peer)` lists the ids.

## What to look at while it runs

* **Shake** kicks every ball upwards: the budget meter jumps to 100 % and the deferred count climbs; when
  the pile is asleep again both fall back to zero.
* **Balls**: more balls cost more only while they move. A sleeping pile of 600 costs what a pile of 60 does.
* **Trusted paddles**: amber in the overlay; the paddle no longer rolls back, and no longer can be trusted.
* What is not here yet: a ball pushed by a client's paddle is the server's, so the client sees it react
  late. WP15 (docs/PLAN_NETCODE.md section 4) hands touched bodies to the pushing client for a while.
