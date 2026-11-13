# Doors: players travel between scenes

The first of gdnet's two instructional demos (docs/PLAN_NETCODE.md 2.11, WP13). A hub with two doors, a
cave and a garden behind them, a chat box that every room shares. Each level is an ordinary scene with a
`NetManager` inside; players walk through doors and land in another level, and a peer only receives the
entities of the level its player is in. Read it top to bottom: every file of the demo is listed here, in
full (a test, `tests/integration/test_doors_demo.gd`, checks that these listings are the files).

The concepts are in `docs/guide/09_rooms_and_travel.md`; this page is the worked example.

## Running it

* **In the editor**: open `demos/doors/main.tscn` and press *Run Current Scene* (F6). Choose **Offline** to
  walk around alone (the host is a client of itself), **Host** to host on port 7350, **Join** to join the
  address in the box.
* **Two clients on one machine**: *Debug > Customize Run Instances...*, enable *Enable Multiple
  Instances*, set the count to 3, and give the first instance the launch argument `-- --server` (the
  dedicated server, see below). Run `main.tscn`; press **Join** in the two other windows.
* **A headless server**: `godot --headless --path . demos/doors/main.tscn -- --server`. Everything after `--`
  reaches the game through `OS.get_cmdline_user_args()`; `main.gd` sees `--server`, hosts without a
  local player and never shows a window's worth of buttons.
* **The tests**: `python3 tools/check.py --only integration --filter doors` runs the whole demo headless, a
  server and two or three clients in one process, as a host-player and as a dedicated server.

Move with the arrow keys or WASD (the project's `left`, `right`, `up`, `down` actions).

## The title screen: `main.tscn` and `main.gd`

`main.tscn` holds the menu, the `World` node and the chat. Nothing in it is networked.

```text file=demos/doors/main.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/main.gd" id="1_main"]
[ext_resource type="Script" path="res://demos/doors/world.gd" id="2_world"]
[ext_resource type="PackedScene" path="res://demos/doors/chat.tscn" id="3_chat"]

[node name="Main" type="Control"]
layout_mode = 3
anchors_preset = 15
anchor_right = 1.0
anchor_bottom = 1.0
script = ExtResource("1_main")

[node name="World" type="Node2D" parent="."]
script = ExtResource("2_world")

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
text = "Doors"
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

[node name="Chat" parent="." instance=ExtResource("3_chat")]
visible = false
layout_mode = 0
```

`main.gd` starts a session with one of `Net`'s four calls and then tells the World and the chat that a
session runs. `--server` (after `--` on the command line) makes this process a dedicated server.

```gdscript file=demos/doors/main.gd
extends Control
## The title screen: Host, Join or Offline. `--server` after `--` on the command line starts a
## dedicated server without a window's worth of buttons: `godot --headless --path . demos/doors/main.tscn -- --server`.

const PORT := 7350

@onready var world: DoorsWorld = $World
@onready var menu: Control = $Menu
@onready var chat: Control = $Chat


func _ready() -> void:
	$Menu/Host.pressed.connect(func() -> void: _start(Net.host(PORT)))
	$Menu/Join.pressed.connect(func() -> void: _start(Net.join($Menu/Address.text, PORT)))
	$Menu/Offline.pressed.connect(func() -> void: _start(Net.offline()))
	if "--server" in OS.get_cmdline_user_args():
		world.dedicated = true
		_start(Net.host(PORT))


func _start(error: Error) -> void:
	if error != OK:
		push_error("could not start the session: %s" % error_string(error))
		return
	menu.hide()
	chat.show()
	world.start()
	chat.start()
```

## The World node: `world.gd`

Levels are loaded under one node the game owns, the *World*: `Net.set_world(self)` registers it. On the
server every level that has a player in it is loaded here at once, each with its own `NetManager`; a client
holds only the level its player is in, because gdnet loads and frees levels for it when the server sends
it somewhere. A level node is named after its scene file (`hub.tscn` becomes `hub`), so its manager sits
at the same path on every peer.

`start()` runs on every peer once a session is up; only the server does anything. It loads the start
level (`load_level` returns the level's root, and loading a loaded level just returns it), lets everyone
write under `chat/` in the keyed state, and gives each peer a player with `Net.spawn_for`. The first
entity spawned for a peer is its *player*: the room of the manager holding it is the peer's room, and the
peer receives the entities of that room only. The client of a new peer is told to load the hub before
anything of the hub reaches it.

`Net.session_for(self)` is the session serving this node's branch: in the game it is `Net`'s own session,
in the tests each simulated peer has its own.

```gdscript file=demos/doors/world.gd
class_name DoorsWorld
extends Node2D
## The World node: every level this peer shows is loaded under it. The server holds every level
## someone is in; a client holds only the level its player is in (docs/guide/09_rooms_and_travel.md).

const PLAYER := preload("res://demos/doors/player.tscn")

## The level new players start in.
@export_file("*.tscn") var start_level: String = "res://demos/doors/hub.tscn"
## True on a dedicated server: peer 1 gets no player.
@export var dedicated: bool = false

var _players: Dictionary = {}


func _ready() -> void:
	Net.set_world(self)


## Called once the session runs. The server loads the start level, opens the chat to everyone and
## gives every peer a player; a client waits for the server to tell it where its player is.
func start() -> void:
	var session: NetSession = Net.session_for(self)
	if not session.is_server():
		return
	session.load_level(start_level)
	session.state.allow("chat/", "all")
	session.peer_joined.connect(spawn_player)
	session.peer_left.connect(remove_player)
	if not dedicated:
		spawn_player(1)
	for id in session.peers():
		spawn_player(id)


## Server: a player for peer `id`, at the start level's `Entrance`.
func spawn_player(id: int) -> void:
	var level: Node = Net.session_for(self).load_level(start_level)
	var player: Node2D = Net.spawn_for(id, PLAYER, level.get_node("NetManager"), "Player%d" % id)
	player.global_position = (level.get_node("Entrance") as Node2D).global_position
	_players[id] = player


func remove_player(id: int) -> void:
	if is_instance_valid(_players.get(id)):
		_players[id].queue_free()
	_players.erase(id)
```

## The levels: `hub.tscn`, `cave.tscn`, `garden.tscn`

Each level is a `Node2D` with a floor, a `NetManager`, doors and `Marker2D`s where players arrive. A
manager's `room` defaults to the scene it sits in, so the hub's room is `res://demos/doors/hub.tscn`
without setting anything. The torches under the managers are level content: every peer that loads the
level has them, and gdnet binds each peer's copy to the server's entity (rung 0, no script). Doors sit
outside the manager: they are not networked, they only react on the server.

```text file=demos/doors/hub.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/door.gd" id="1_door"]
[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="2_torch"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_door"]
size = Vector2(48, 96)

[node name="Hub" type="Node2D"]

[node name="Floor" type="ColorRect" parent="."]
offset_left = -400.0
offset_top = -300.0
offset_right = 400.0
offset_bottom = 300.0
color = Color(0.25, 0.25, 0.3, 1)

[node name="NetManager" type="NetManager" parent="."]

[node name="TorchLeft" parent="NetManager" instance=ExtResource("2_torch")]
position = Vector2(-300, -250)

[node name="TorchRight" parent="NetManager" instance=ExtResource("2_torch")]
position = Vector2(300, -250)

[node name="CaveDoor" type="Area2D" parent="."]
position = Vector2(-370, 0)
collision_layer = 0
collision_mask = 2
script = ExtResource("1_door")
target_scene = "res://demos/doors/cave.tscn"

[node name="Shape" type="CollisionShape2D" parent="CaveDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="GardenDoor" type="Area2D" parent="."]
position = Vector2(370, 0)
collision_layer = 0
collision_mask = 2
script = ExtResource("1_door")
target_scene = "res://demos/doors/garden.tscn"

[node name="Shape" type="CollisionShape2D" parent="GardenDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="Entrance" type="Marker2D" parent="."]

[node name="FromCave" type="Marker2D" parent="."]
position = Vector2(-280, 0)

[node name="FromGarden" type="Marker2D" parent="."]
position = Vector2(280, 0)
```

```text file=demos/doors/cave.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/door.gd" id="1_door"]
[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="2_torch"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_door"]
size = Vector2(96, 48)

[node name="Cave" type="Node2D"]

[node name="Floor" type="ColorRect" parent="."]
offset_left = -300.0
offset_top = -200.0
offset_right = 300.0
offset_bottom = 200.0
color = Color(0.15, 0.12, 0.1, 1)

[node name="NetManager" type="NetManager" parent="."]

[node name="CaveTorch" parent="NetManager" instance=ExtResource("2_torch")]
position = Vector2(0, -150)

[node name="ReturnDoor" type="Area2D" parent="."]
position = Vector2(0, 175)
collision_layer = 0
collision_mask = 2
script = ExtResource("1_door")
target_scene = "res://demos/doors/hub.tscn"
spawn_point = "FromCave"

[node name="Shape" type="CollisionShape2D" parent="ReturnDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="Entrance" type="Marker2D" parent="."]
position = Vector2(0, 100)
```

```text file=demos/doors/garden.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/door.gd" id="1_door"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_door"]
size = Vector2(96, 48)

[node name="Garden" type="Node2D"]

[node name="Floor" type="ColorRect" parent="."]
offset_left = -300.0
offset_top = -200.0
offset_right = 300.0
offset_bottom = 200.0
color = Color(0.2, 0.45, 0.2, 1)

[node name="NetManager" type="NetManager" parent="."]

[node name="ReturnDoor" type="Area2D" parent="."]
position = Vector2(0, -175)
collision_layer = 0
collision_mask = 2
script = ExtResource("1_door")
target_scene = "res://demos/doors/hub.tscn"
spawn_point = "FromGarden"

[node name="Shape" type="CollisionShape2D" parent="ReturnDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="Entrance" type="Marker2D" parent="."]
position = Vector2(0, -100)
```

`torch.tscn` is the smallest entity there is: a scene instance under a manager replicates its transform,
visibility and colour with no script.

```text file=demos/doors/torch.tscn
[gd_scene format=3]

[node name="Torch" type="Node2D"]

[node name="Flame" type="Polygon2D" parent="."]
color = Color(1, 0.6, 0.1, 1)
polygon = PackedVector2Array(0, -16, 8, 0, 0, 8, -8, 0)
```

## Doors: `door.gd`

A door is an `Area2D`. When a player's body enters it on the server, `Net.travel(body, target_scene,
spawn_point)` sends that player to the other level. The call is queued and applied at the next network
tick, so it is safe inside `body_entered` (moving a physics body inside a physics callback is refused by
the engine). On the server gdnet loads the target level if it is not loaded, moves the player under its
`NetManager` at the marker named `spawn_point`, and tells the player's client to load that level; the
client acknowledges, applies the move once the level is there, and frees its previous level when the
server says the player left that room (and none of its entities remain in it). Other peers stop or start
seeing the player as it leaves or enters their room.

The level check matters on a server that holds several levels: they all share one physics space (they
overlap at the origin unless the project setting `gdnet/levels/spacing` places them apart, see
`docs/guide/09_rooms_and_travel.md`), so a door ignores players of other levels. `multiplayer.is_server()` is Godot's own
query; it is false on every client.

```gdscript file=demos/doors/door.gd
extends Area2D
## A door. When a player walks in, the server sends that player to `target_scene`, where it arrives
## at the marker named `spawn_point`.

## The level this door leads to.
@export_file("*.tscn") var target_scene: String = ""
## The Marker2D of that level the player arrives at.
@export var spawn_point: String = "Entrance"


func _ready() -> void:
	body_entered.connect(_on_body_entered)


func _on_body_entered(body: Node2D) -> void:
	# Only the server moves players, and only players of this door's own level: a server holding
	# several levels has them all in one physics space.
	if multiplayer.is_server() and body is DoorsPlayer and owner.is_ancestor_of(body):
		Net.travel(body, target_scene, spawn_point)
```

## The player: `player.tscn` and `player.gd`

Rung 1's predicted player: `Net.input(self)` instead of `Input`, and `spawn_for` gave it an owner, so its
owner predicts it and everyone else interpolates it. Players are on collision layer 2 and collide only with
layer 1, so players of different peers never push each other. The camera follows the owner's player only.
`_net_travelled(from, to)` runs on every part of an entity after it moved to another room (on the server
and on the clients that still see it, the owner included), with the two room names; the camera jumps
instead of gliding across the level. Prediction and interpolation of the player restart after a travel.

```text file=demos/doors/player.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/player.gd" id="1_player"]

[sub_resource type="CircleShape2D" id="CircleShape2D_body"]
radius = 12.0

[node name="Player" type="CharacterBody2D"]
collision_layer = 2
collision_mask = 1
script = ExtResource("1_player")

[node name="Shape" type="CollisionShape2D" parent="."]
shape = SubResource("CircleShape2D_body")

[node name="Body" type="Polygon2D" parent="."]
color = Color(0.3, 0.7, 1, 1)
polygon = PackedVector2Array(0, -12, 12, 0, 0, 12, -12, 0)

[node name="Camera2D" type="Camera2D" parent="."]
enabled = false
position_smoothing_enabled = true
```

```gdscript file=demos/doors/player.gd
class_name DoorsPlayer
extends CharacterBody2D
## Rung 1's predicted player (docs/PLAN_NETCODE.md section 1) with a camera that follows its owner.

const SPEED := 200.0


func _net_spawned(_info: Dictionary) -> void:
	$Camera2D.enabled = Net.is_owner(self)


func _physics_process(_delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()


## After a door: the player stands in another level, so the camera jumps instead of gliding there.
func _net_travelled(_from: String, _to: String) -> void:
	$Camera2D.reset_smoothing()
```

## The chat: `chat.tscn` and `chat.gd`

Messages are paths under `chat/` in the keyed state (`Net.state` in a game; the branch's
`NetSession.state` here, through `Net.session_for`). The keyed state is global, unlike entities, so a
message written in the cave is read in the hub. The server allowed everyone to write under `chat/` in
`world.gd`; a message's path is the count of messages so far, so two messages sent in the same instant can
overwrite each other (good enough for a demo; give each peer its own prefix with `allow("chat/", "owner")`
and paths `chat/<peer id>/<n>` for a real game).

```text file=demos/doors/chat.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/chat.gd" id="1_chat"]

[node name="Chat" type="PanelContainer"]
offset_left = 8.0
offset_top = 8.0
offset_right = 328.0
offset_bottom = 208.0
script = ExtResource("1_chat")

[node name="Box" type="VBoxContainer" parent="."]
layout_mode = 2

[node name="Log" type="RichTextLabel" parent="Box"]
layout_mode = 2
size_flags_vertical = 3
scroll_following = true

[node name="Input" type="LineEdit" parent="Box"]
layout_mode = 2
placeholder_text = "Say something, then Enter"
```

```gdscript file=demos/doors/chat.gd
extends PanelContainer
## A chat box on the keyed state. Every message is a path under `chat/`, and the keyed state is
## global, so a message reaches every room.

@onready var log_label: RichTextLabel = $Box/Log
@onready var input: LineEdit = $Box/Input

var _state: NetKeyedState


## Called once the session runs.
func start() -> void:
	_state = Net.session_for(self).state
	_state.changed.connect(_on_changed)
	for path in _state.keys("chat/"):
		_on_changed(path, _state.get(path))
	input.text_submitted.connect(send)


func send(text: String) -> void:
	if text.strip_edges().is_empty():
		return
	var path := "chat/%06d" % _state.keys("chat/").size()
	_state.set(path, "%d: %s" % [multiplayer.get_unique_id(), text])
	input.clear()


func _on_changed(path: String, value: Variant) -> void:
	if path.begins_with("chat/"):
		log_label.append_text("%s\n" % value)
```

## What to look at while it runs

* **The meter** (F9, or `Net.show_meter(true)`) shows the local room, the entities per room and whether a
  travel is in flight (`rooms` line), and `entities in scope`: a client holds only its room's entities.
* **The overlay** (`Net.show_overlay(true)`) lists every entity with its room.
* `Net.levels()` on the server lists every loaded level; a level with nobody in it is unloaded 30 seconds
  after its last player left (`gdnet/levels/empty_timeout_s`, or `Net.empty_level_timeout`). The server
  keeps its state and puts it back when the level loads again (`Net.kept_levels()`; guide page 9, *Level
  state*).
