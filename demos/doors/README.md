# Doors: an overworld of chunks, and interiors behind doors

The first of gdnet's instructional demos (docs/PLAN_NETCODE.md 2.11 and 12.2, WP13 and WP16). An overworld
of four chunk scenes in a 2 x 2 grid, which players walk across without a loading screen; a cave and a
garden behind doors; torches that only nearby players receive; a chest in the cave that stays open; a chat
box every room shares. Each level is an ordinary scene with a `NetManager` inside. Read it top to bottom:
every file of the demo is listed here, in full (a test, `tests/integration/test_doors_demo.gd`, checks that
these listings are the files).

The concepts are in `docs/guide/09_rooms_and_travel.md` (rooms, chunks, placement, level state) and
`docs/guide/11_visibility.md` (radius); this page is the worked example.

## Running it

* **In the editor**: open `demos/doors/main.tscn` and press *Run Current Scene* (F6). Choose **Offline** to
  walk around alone (the host is a client of itself), **Host** to host on port 7350, **Join** to join the
  address in the box.
* **Over the internet, through Steam**: with GodotSteam installed (`tools/install_godotsteam.sh`) and the
  Steam client running, the menu also shows **Host on Steam**, which shows a six-character join code; the
  other player types the code into the same box and presses **Join**. No port forwarding, no server of
  your own. `docs/guide/12_playing_online.md` explains it, including how to send a build to a tester.
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

`main.tscn` holds the menu, the `World` node and the chat. Nothing in it is networked. The menu is gdnet's
shared title screen, `addons/gdnet/connect/connect_menu.tscn` (the connect layer, guide page 12), given
the demo's title and port; the Ballpit demo uses the same one.

```text file=demos/doors/main.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/main.gd" id="1_main"]
[ext_resource type="Script" path="res://demos/doors/world.gd" id="2_world"]
[ext_resource type="PackedScene" path="res://demos/doors/chat.tscn" id="3_chat"]
[ext_resource type="PackedScene" path="res://addons/gdnet/connect/connect_menu.tscn" id="4_menu"]

[node name="Main" type="Control"]
layout_mode = 3
anchors_preset = 15
anchor_right = 1.0
anchor_bottom = 1.0
script = ExtResource("1_main")

[node name="World" type="Node2D" parent="."]
script = ExtResource("2_world")

[node name="Menu" parent="." instance=ExtResource("4_menu")]
title = "Doors"
port = 7350

[node name="Chat" parent="." instance=ExtResource("3_chat")]
visible = false
layout_mode = 0
```

`main.gd` waits for the menu's `started` signal, which fires once a session runs, whichever button
started it (ENet, Steam or offline), and then tells the World and the chat that a session runs. The game
code after that is the same for every transport. `--server` (after `--` on the command line) makes this
process a dedicated server: `menu.host()` hosts over ENet on the menu's port without a click.

```gdscript file=demos/doors/main.gd
extends Control
## The title screen is gdnet's connect menu (`addons/gdnet/connect/`): Host, Host on Steam, Join (an
## address or a join code) or Offline. `--server` after `--` on the command line starts a dedicated server
## without a window's worth of buttons: `godot --headless --path . demos/doors/main.tscn -- --server`.

@onready var world: DoorsWorld = $World
@onready var menu: NetConnectMenu = $Menu
@onready var chat: Control = $Chat


func _ready() -> void:
	menu.started.connect(_start)
	if "--server" in OS.get_cmdline_user_args():
		world.dedicated = true
		menu.host()


func _start() -> void:
	chat.show()
	world.start()
	chat.start()
```

## The World node: `world.gd`

Levels are loaded under one node the game owns, the *World*: `Net.set_world(self)` registers it. On the
server every level someone needs is loaded here at once, each with its own `NetManager`; a client holds
the levels of its rooms, because gdnet loads and frees levels for it as the server tells it. A level node
is named after its scene file (`chunk_0_0.tscn` becomes `chunk_0_0`), so its manager sits at the same path
on every peer.

`start()` runs on every peer once a session is up; only the server does anything. It loads the start
level (`load_level` returns the level's root, and loading a loaded level just returns it), lets everyone
write under `chat/` in the keyed state, and gives each peer a player with `Net.spawn_for`. The first
entity spawned for a peer is its *player*: the room of the manager holding it is the peer's room. The
client of a new peer is told to load its levels before anything in them reaches it.

`Net.session_for(self)` is the session serving this node's branch: in the game it is `Net`'s own session,
in the tests each simulated peer has its own.

```gdscript file=demos/doors/world.gd
class_name DoorsWorld
extends Node2D
## The World node: every level this peer shows is loaded under it. The server holds every level
## someone needs; a client holds its player's level and that level's neighbours
## (docs/guide/09_rooms_and_travel.md).

const PLAYER := preload("res://demos/doors/player.tscn")

## The level new players start in.
@export_file("*.tscn") var start_level: String = "res://demos/doors/chunk_0_0.tscn"
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


## Server: a player for peer `id`, at the start level's `Entrance` and turned its way, as a door's
## `Net.travel` places it.
func spawn_player(id: int) -> void:
	var level: Node = Net.session_for(self).load_level(start_level)
	var player: Node2D = Net.spawn_for(id, PLAYER, level.get_node("NetManager"), "Player%d" % id)
	var entrance: Node2D = level.get_node("Entrance")
	player.global_position = entrance.global_position
	player.global_rotation = entrance.global_rotation
	_players[id] = player


func remove_player(id: int) -> void:
	if is_instance_valid(_players.get(id)):
		_players[id].queue_free()
	_players.erase(id)
```

## The overworld: four chunks

The overworld is four scenes, `chunk_0_0` to `chunk_1_1`, each 1,200 x 800 px, authored where they sit in
the world: `chunk_1_0`'s root is at (1200, 0), `chunk_0_1`'s at (0, 800). Each has a floor, walls on its
outer sides, torches, and a `NetManager` with three settings:

* `neighbours`: the other three chunks. A peer's rooms are its player's room plus that room's neighbours,
  so a player anywhere in the overworld has all four: its client loads all four levels, receives their
  entities and collides with their geometry. The boulder of `chunk_1_0` straddles the edge with
  `chunk_0_0`, and a player in `chunk_0_0` bumps into it before entering `chunk_1_0`.
* `bounds = Rect2(0, 0, 1200, 800)`: the chunk's area, in the coordinates of its root.
* `auto_travel = true`: on the server, a player whose position enters a chunk's bounds travels to that
  chunk's manager, keeping its position. No script at the edge is needed.

Walking from one chunk into the next is therefore a move between two rooms every peer already has: the
server sends a `Reparent`, each peer moves its copy of the player under the other manager, no replica is
freed and the owner's prediction keeps running, so nothing rolls back. The test
`test_g9_crossing_a_chunk_edge_frees_no_replica` checks exactly that.

The entities under a `NetManager` use global coordinates (a manager is a plain `Node`, so its children do
not inherit the root's position): the torches of `chunk_1_0` are at x = 1400 to 2200. Geometry outside the
manager (floor, walls, boulder, doors, markers) is relative to the root as usual.

New players start at the crossroads, the `Entrance` of `chunk_0_0` near the point where the four chunks
meet. The cave's door is in `chunk_0_0`, the garden's in `chunk_1_1`.

```text file=demos/doors/chunk_0_0.tscn
[gd_scene format=3]

[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="1_torch"]
[ext_resource type="Script" path="res://demos/doors/door.gd" id="2_door"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_long"]
size = Vector2(1200, 40)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_side"]
size = Vector2(40, 800)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_door"]
size = Vector2(48, 96)

[node name="Chunk00" type="Node2D"]

[node name="Floor" type="ColorRect" parent="."]
offset_right = 1200.0
offset_bottom = 800.0
color = Color(0.3, 0.5, 0.25, 1)

[node name="Walls" type="StaticBody2D" parent="."]

[node name="North" type="CollisionShape2D" parent="Walls"]
position = Vector2(600, -20)
shape = SubResource("RectangleShape2D_long")

[node name="West" type="CollisionShape2D" parent="Walls"]
position = Vector2(-20, 400)
shape = SubResource("RectangleShape2D_side")

[node name="NetManager" type="NetManager" parent="."]
neighbours = PackedStringArray("res://demos/doors/chunk_0_1.tscn", "res://demos/doors/chunk_1_0.tscn", "res://demos/doors/chunk_1_1.tscn")
auto_travel = true
bounds = Rect2(0, 0, 1200, 800)

[node name="Torch1" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(200, 200)

[node name="Torch2" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(600, 200)

[node name="Torch3" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1000, 200)

[node name="Torch4" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(200, 600)

[node name="Torch5" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(600, 600)

[node name="Torch6" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1000, 600)

[node name="CaveDoor" type="Area2D" parent="."]
position = Vector2(200, 400)
collision_layer = 0
collision_mask = 2
script = ExtResource("2_door")
target_scene = "res://demos/doors/cave.tscn"

[node name="Shape" type="CollisionShape2D" parent="CaveDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="Entrance" type="Marker2D" parent="."]
position = Vector2(1140, 740)

[node name="FromCave" type="Marker2D" parent="."]
position = Vector2(300, 400)
```

```text file=demos/doors/chunk_1_0.tscn
[gd_scene format=3]

[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="1_torch"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_long"]
size = Vector2(1200, 40)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_side"]
size = Vector2(40, 800)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_boulder"]
size = Vector2(80, 80)

[node name="Chunk10" type="Node2D"]
position = Vector2(1200, 0)

[node name="Floor" type="ColorRect" parent="."]
offset_right = 1200.0
offset_bottom = 800.0
color = Color(0.35, 0.55, 0.3, 1)

[node name="Walls" type="StaticBody2D" parent="."]

[node name="North" type="CollisionShape2D" parent="Walls"]
position = Vector2(600, -20)
shape = SubResource("RectangleShape2D_long")

[node name="East" type="CollisionShape2D" parent="Walls"]
position = Vector2(1220, 400)
shape = SubResource("RectangleShape2D_side")

[node name="Boulder" type="StaticBody2D" parent="."]
position = Vector2(0, 250)

[node name="Shape" type="CollisionShape2D" parent="Boulder"]
shape = SubResource("RectangleShape2D_boulder")

[node name="Look" type="Polygon2D" parent="Boulder"]
color = Color(0.45, 0.42, 0.4, 1)
polygon = PackedVector2Array(-40, -40, 40, -40, 40, 40, -40, 40)

[node name="NetManager" type="NetManager" parent="."]
neighbours = PackedStringArray("res://demos/doors/chunk_0_0.tscn", "res://demos/doors/chunk_0_1.tscn", "res://demos/doors/chunk_1_1.tscn")
auto_travel = true
bounds = Rect2(0, 0, 1200, 800)

[node name="Torch1" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1400, 200)

[node name="Torch2" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1800, 200)

[node name="Torch3" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(2200, 200)

[node name="Torch4" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1400, 600)

[node name="Torch5" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1800, 600)

[node name="Torch6" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(2200, 600)
```

```text file=demos/doors/chunk_0_1.tscn
[gd_scene format=3]

[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="1_torch"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_long"]
size = Vector2(1200, 40)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_side"]
size = Vector2(40, 800)

[node name="Chunk01" type="Node2D"]
position = Vector2(0, 800)

[node name="Floor" type="ColorRect" parent="."]
offset_right = 1200.0
offset_bottom = 800.0
color = Color(0.4, 0.5, 0.25, 1)

[node name="Walls" type="StaticBody2D" parent="."]

[node name="South" type="CollisionShape2D" parent="Walls"]
position = Vector2(600, 820)
shape = SubResource("RectangleShape2D_long")

[node name="West" type="CollisionShape2D" parent="Walls"]
position = Vector2(-20, 400)
shape = SubResource("RectangleShape2D_side")

[node name="NetManager" type="NetManager" parent="."]
neighbours = PackedStringArray("res://demos/doors/chunk_0_0.tscn", "res://demos/doors/chunk_1_0.tscn", "res://demos/doors/chunk_1_1.tscn")
auto_travel = true
bounds = Rect2(0, 0, 1200, 800)

[node name="Torch1" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(200, 1000)

[node name="Torch2" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(600, 1000)

[node name="Torch3" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1000, 1000)

[node name="Torch4" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(200, 1400)

[node name="Torch5" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(600, 1400)

[node name="Torch6" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1000, 1400)
```

```text file=demos/doors/chunk_1_1.tscn
[gd_scene format=3]

[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="1_torch"]
[ext_resource type="Script" path="res://demos/doors/door.gd" id="2_door"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_long"]
size = Vector2(1200, 40)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_side"]
size = Vector2(40, 800)

[sub_resource type="RectangleShape2D" id="RectangleShape2D_door"]
size = Vector2(48, 96)

[node name="Chunk11" type="Node2D"]
position = Vector2(1200, 800)

[node name="Floor" type="ColorRect" parent="."]
offset_right = 1200.0
offset_bottom = 800.0
color = Color(0.3, 0.45, 0.3, 1)

[node name="Walls" type="StaticBody2D" parent="."]

[node name="South" type="CollisionShape2D" parent="Walls"]
position = Vector2(600, 820)
shape = SubResource("RectangleShape2D_long")

[node name="East" type="CollisionShape2D" parent="Walls"]
position = Vector2(1220, 400)
shape = SubResource("RectangleShape2D_side")

[node name="NetManager" type="NetManager" parent="."]
neighbours = PackedStringArray("res://demos/doors/chunk_0_0.tscn", "res://demos/doors/chunk_0_1.tscn", "res://demos/doors/chunk_1_0.tscn")
auto_travel = true
bounds = Rect2(0, 0, 1200, 800)

[node name="Torch1" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1400, 1000)

[node name="Torch2" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1800, 1000)

[node name="Torch3" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(2200, 1000)

[node name="Torch4" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1400, 1400)

[node name="Torch5" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(1800, 1400)

[node name="Torch6" parent="NetManager" instance=ExtResource("1_torch")]
position = Vector2(2200, 1400)

[node name="GardenDoor" type="Area2D" parent="."]
position = Vector2(1000, 400)
collision_layer = 0
collision_mask = 2
script = ExtResource("2_door")
target_scene = "res://demos/doors/garden.tscn"

[node name="Shape" type="CollisionShape2D" parent="GardenDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="FromGarden" type="Marker2D" parent="."]
position = Vector2(900, 400)
```

## Interiors and placement: `cave.tscn`, `garden.tscn`

An interior is a level without neighbours: a player inside has that one room, and its client frees the
overworld while it is inside. Interiors are authored around the origin like any scene. Loaded as they are,
they would overlap `chunk_0_0`, so `project.godot` sets `gdnet/levels/spacing` to 4000: a level without
neighbours that is loaded without an origin takes the next free cell of a grid along +x, at 4000, 8000,
and so on (a chunk, which names neighbours, keeps its authored place). The first interior a player enters
sits at (4000, 0) on the server and on every client; the origin travels with the `Travel` message. The
cell is freed when the level unloads.

Placement is why the levels on one server, which share one physics space, never touch: a door only ever
sees players of its own level, and a body in the cave cannot hit the overworld's walls.

The cave holds a torch and the chest of *Level state* below; the garden is empty. Their doors lead back to
markers in the overworld.

```text file=demos/doors/cave.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/door.gd" id="1_door"]
[ext_resource type="PackedScene" path="res://demos/doors/torch.tscn" id="2_torch"]
[ext_resource type="PackedScene" path="res://demos/doors/chest.tscn" id="3_chest"]

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

[node name="Chest" parent="NetManager" instance=ExtResource("3_chest")]
position = Vector2(200, -120)

[node name="ReturnDoor" type="Area2D" parent="."]
position = Vector2(0, 175)
collision_layer = 0
collision_mask = 2
script = ExtResource("1_door")
target_scene = "res://demos/doors/chunk_0_0.tscn"
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
target_scene = "res://demos/doors/chunk_1_1.tscn"
spawn_point = "FromGarden"

[node name="Shape" type="CollisionShape2D" parent="ReturnDoor"]
shape = SubResource("RectangleShape2D_door")

[node name="Entrance" type="Marker2D" parent="."]
position = Vector2(0, -100)
```

## Doors: `door.gd`

A door is an `Area2D`. When a player's body enters it on the server, `Net.travel(body, target_scene,
spawn_point)` sends that player to the other level. The call is queued and applied at the next network
tick, so it is safe inside `body_entered` (moving a physics body inside a physics callback is refused by
the engine). On the server gdnet loads the target level if it is not loaded (at its grid cell), moves the
player under its `NetManager` to the marker named `spawn_point` (its position and its rotation: the
player faces the way the marker points), and tells the player's client to load that level; the client
loads it, applies the move once it is there, and frees the levels the player left. Other peers stop or
start seeing the player as it leaves or enters their rooms. The player's `rotation` is replicated (every
`Node2D`'s is, by default), so every peer sees it turned. `multiplayer.is_server()` is Godot's own query;
it is false on every client.

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
	# Only the server moves players.
	if multiplayer.is_server() and body is DoorsPlayer:
		Net.travel(body, target_scene, spawn_point)
```

## Radius: `torch.tscn`

Twenty-four torches light the overworld. Each has a `NetEntity` with `radius = 400`: a peer receives a
torch only while it is within 400 px of the peer's player, and loses it again beyond 480 px (1.2 times the
radius, so a torch at the edge is not created and freed in turn). A player at the crossroads holds the
four torches near it, not the other twenty, although its client has all four chunks loaded. A torch it
walks towards arrives with a fresh `Create`; one it leaves behind is freed on its client.

`inherit_profile = true` keeps the scene's inferred properties (transform, visibility, colour); the
`NetEntity` only adds the radius. With no radius anywhere a game pays nothing for this.

```text file=demos/doors/torch.tscn
[gd_scene format=3]

[node name="Torch" type="Node2D"]

[node name="Flame" type="Polygon2D" parent="."]
color = Color(1, 0.6, 0.1, 1)
polygon = PackedVector2Array(0, -16, 8, 0, 0, 8, -8, 0)

[node name="NetEntity" type="NetEntity" parent="."]
inherit_profile = true
radius = 400.0
```

## Level state: `chest.tscn` and `chest.gd`

The chest in the cave opens when a player walks into it. `open` is an exported variable, so gdnet
replicates it like any exported property, and its setter updates the lid so the change shows on every
peer.

When everyone has left the cave, the server unloads it after `empty_level_timeout` (30 s by default). The
cave keeps its state: the server remembers every replicated property of the level's entities, and the next
time the cave loads the chest is put back open before anything of it is sent. A client entering the cave
again gets the chest open in its `Create`. The setter handles that too: the kept value is set after
`_ready`, and the setter repaints the lid. `Net.level_state(path)` returns the same state as bytes, for a
game that wants it in its save file.

```text file=demos/doors/chest.tscn
[gd_scene format=3]

[ext_resource type="Script" path="res://demos/doors/chest.gd" id="1_chest"]

[sub_resource type="RectangleShape2D" id="RectangleShape2D_chest"]
size = Vector2(40, 28)

[node name="Chest" type="Area2D"]
collision_layer = 0
collision_mask = 2
script = ExtResource("1_chest")

[node name="Shape" type="CollisionShape2D" parent="."]
shape = SubResource("RectangleShape2D_chest")

[node name="Lid" type="Polygon2D" parent="."]
color = Color(0.55, 0.35, 0.15, 1)
polygon = PackedVector2Array(-20, -14, 20, -14, 20, 14, -20, 14)
```

```gdscript file=demos/doors/chest.gd
class_name DoorsChest
extends Area2D
## A chest that opens when a player walks into it, on the server. `open` is exported, so it
## replicates, and the server keeps it when the level unloads: the chest is still open next time.

const SHUT := Color(0.55, 0.35, 0.15, 1)
const OPENED := Color(1, 0.85, 0.3, 1)

## Whether the lid is open.
@export var open: bool = false:
	set(value):
		open = value
		_show()


func _ready() -> void:
	body_entered.connect(_on_body_entered)
	_show()


func _show() -> void:
	if is_node_ready():
		($Lid as Polygon2D).color = OPENED if open else SHUT


func _on_body_entered(body: Node2D) -> void:
	if multiplayer.is_server() and body is DoorsPlayer:
		open = true
```

## The player: `player.tscn` and `player.gd`

Rung 1's predicted player: `Net.input(self)` instead of `Input`, and `spawn_for` gave it an owner, so its
owner predicts it and everyone else interpolates it. Players are on collision layer 2 and collide only with
layer 1, so players of different peers never push each other. The camera follows the owner's player only.
`_net_travelled(from, to)` runs on every part of an entity after it moved to another room (on the server
and on the clients that see it, the owner included), with the two room names. Through a door the camera
jumps instead of gliding across the world; crossing a chunk edge is a travel too, and there the player
has not moved, so the camera keeps gliding.

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


## After a door the player stands somewhere else, so the camera jumps instead of gliding there. A walk
## into the next chunk is a travel too, and the camera keeps gliding.
func _net_travelled(_from: String, _to: String) -> void:
	var camera: Camera2D = $Camera2D
	if camera.get_screen_center_position().distance_to(global_position) > 600.0:
		camera.reset_smoothing()
```

## The chat: `chat.tscn` and `chat.gd`

Messages are paths under `chat/` in the keyed state (`Net.state` in a game; the branch's
`NetSession.state` here, through `Net.session_for`). The keyed state is global, unlike entities, so a
message written in the cave is read in the overworld. The server allowed everyone to write under `chat/`
in `world.gd`; a message's path is the count of messages so far, so two messages sent in the same instant
can overwrite each other (good enough for a demo; give each peer its own prefix with
`allow("chat/", "owner")` and paths `chat/<peer id>/<n>` for a real game).

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

* **The meter** (F9, or `Net.show_meter(true)`) shows the local room and how many other rooms the peer has
  (`room chunk_0_0 +3` in the overworld), the entities per room and whether a travel is in flight
  (`rooms` line), and `entities in scope`: walk around the overworld and watch it change as torches come
  and go.
* **The overlay** (`Net.show_overlay(true)`) lists every entity with its room.
* `Net.levels()` on the server lists every loaded level, `Net.level_origin(path)` where each sits, and
  `Net.kept_levels()` the unloaded levels whose state the server keeps.
