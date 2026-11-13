# 9. Rooms and travel: levels, chunks, interiors and moving between them

A game with more than one level needs three things from the network layer: the server keeps every level
someone uses, each client loads the levels its player needs, and a peer receives only the entities of
those levels. gdnet does this with *rooms* (`docs/PLAN_NETCODE.md` 2.11 and 12.2, WP13 and WP16). The worked
example is the Doors demo, `demos/doors/README.md`.

This page covers rooms and the sets of rooms a peer sees, the World node, travel, chunk overworlds with
`neighbours`, `add_room`, where levels are placed, and `bounds` with `auto_travel`.

## Rooms

Every `NetManager` has a `room`. Leave it empty and it is the scene the manager was saved in (its owner's
`scene_file_path`): a manager inside `res://levels/cave.tscn` is in the room `res://levels/cave.tscn`.
A manager with no scene (added by code) uses its path relative to the session's branch. Set it in the
inspector to group several managers into one room, or to `"*"`: a `"*"` manager is visible from every
room (scoreboards, a global weather entity).

A peer's **primary room** is the room of the manager holding its *player*. The first entity
`Net.spawn_for`'d to a peer is its player; `Net.set_player(peer, node)` says so explicitly (before or after
the spawn). `Net.set_room(peer, name)` sets the primary room whatever the player's (spectators, a lobby); an
empty name removes the override.

A peer sees a **set of rooms**:

* its primary room;
* the rooms that the managers of its primary room name in `neighbours` (a `PackedStringArray` of room
  names on `NetManager`);
* the rooms added with `Net.add_room(peer, name)`, until `Net.remove_room(peer, name)`.

```gdscript
Net.add_room(peer_id, "res://levels/arena.tscn")   # the peer also sees the arena (a spectator view)
Net.remove_room(peer_id, "res://levels/arena.tscn")
Net.rooms(peer_id)       # ["res://levels/hub.tscn", "res://levels/arena.tscn"]: the primary room first
Net.room_of(peer_id)     # "res://levels/hub.tscn"
```

A level's room is its scene path, so the names in `neighbours` and `add_room` are usually scene paths.
When a room in some peer's set is a scene that is not loaded, the server loads it.

## Interest by room

A peer sees an entity when the entity's topmost entity is owned by that peer, or its manager's room is in
the peer's set or is `"*"`, or its manager has no room registered. The server keeps, per peer, the set of
entities it sent it (its *scope*) and filters by it: creates, destroys, moves, owner changes, snapshots,
the owner-state relay, hashes and broadcast calls. When a peer's set changes, or an entity moves to a
manager in another room, the server sends the difference, in this order:

1. `Travel` for each level room the peer gained: its client loads that level (at the level's origin, see
   *Placement*) and answers `TravelAck`;
2. `Scope { leave }` for the entities that left the peer's scope. **The client frees those replicas**
   (their `_net_despawning` runs and `entity_despawning` fires, as for a destroy); they come back with a
   fresh `Create`, current state included, if the peer sees them again;
3. `RoomLeft` for each level room the peer lost: its client frees that level, once no entity it owns is
   still inside it;
4. a burst of `Create`s, parents first, for the entities that entered.

A late joiner's burst covers its rooms only. Before a peer has a room it sees everything, as in a game
with one level, *unless the session uses levels*: once the server called `load_level` or `travel`, a peer
without a room sees only `"*"` managers (so a joining client never receives a level it is about to leave).

The keyed state (`Net.state`, page 7) is global: chat and scores cross rooms.

`NetSession.stats()`: on a client `entities_in_scope` is what it holds; on the server it is every entity,
and `entities_in_scope_by_peer` has each peer's count. `room` is this peer's primary room (on the server:
the host player's) and `my_rooms` its whole set; `rooms` counts entities per room name; `levels` is the
number of loaded levels; `travelling` is, on the server, the `Travel`s not acknowledged yet and, on a
client, the moves waiting for a level to load; `travels`, `scope_entered`, `scope_left`. The meter shows
them on its `rooms` line (`room hub +2` means two more rooms besides the hub); `overlay_rows()` gain
`room`.

## The World pattern

Levels go under one node the game owns, the *World*:

```gdscript
extends Node2D            # world.gd, a node in the main scene

func _ready() -> void:
	Net.set_world(self)   # levels load under this node (in its branch)
```

Without one they go under the session's branch root. A level node is named after its scene file
(`cave.tscn` becomes `cave`), so `World/cave/NetManager` has the same path, and therefore the same manager
key, on every peer.

Server side:

```gdscript
var level: Node = Net.load_level("res://levels/hub.tscn")   # loads it, or returns it when loaded
Net.levels()                                                # ["res://levels/hub.tscn"]
Net.unload_level("res://levels/hub.tscn")                   # destroys its entities, frees it
Net.spawn_for(peer_id, PLAYER, level.get_node("NetManager"))
```

`Net.load_level`, `unload_level`, `levels`, `level_origin`, `set_room`, `add_room`, `remove_room`, `rooms`
and `room_of` act on `Net`'s own session; the same methods exist on `NetSession`, and
`Net.session_for(node)` returns the session of a node's branch (the headless tests run several peers in
one process). A client never loads levels itself: the server tells it.

## Travel

```gdscript
# door.gd (an Area2D), on the server:
func _on_body_entered(body: Node2D) -> void:
	if multiplayer.is_server() and body is Player:
		Net.travel(body, "res://levels/cave.tscn", "Entrance")
```

`Net.travel(player, scene_path, spawn_point = "")` is queued and applied at the next network tick (so it
is safe inside `body_entered`, where the engine refuses to move a body). Then, on the server:

1. the target level is loaded under the World if it is not, and its managers register their rooms;
2. the player's entity (and every entity below it) moves from its manager to the level's manager, at the
   node named `spawn_point` inside the level (a `Marker2D` / `Marker3D`; its global position, so the
   level's origin applies). **Without a spawn point the player keeps its global position**: that is the
   form for walking into the chunk next door;
3. `_net_travelled(from, to)` runs on its parts and `travelled(player, from, to)` fires on `Net` /
   `NetSession`, with the two room names;
4. the owner's rooms change: its client gets the `Travel`s, leaves and creates described above, then the
   `Reparent` of the player.

The client, on `Travel`, instantiates the level under its World at once and answers `TravelAck`; the
`Reparent` waits until the destination manager exists, then moves the player's node into it and fires the
same `_net_travelled` and `travelled`. The client frees its old level on the `RoomLeft` that follows, once
the player has moved out.

Peers that see only the old room see the player leave (a `Scope` leave: the replica is freed); peers that
see only the new room see it arrive (a `Create`). **Peers that see both rooms get only the `Reparent`:** the
replica is moved, not freed and recreated; its interpolation and, on the owner, its prediction keep
running, and nothing rolls back because of the move. The position is continuous because a `NetManager` is a
plain `Node`: the entities under it use global coordinates whichever manager holds them.

The server unloads a level `empty_level_timeout` seconds after nobody needs it (no player entity in any of
its managers, and its room in no peer's set): `NetSession.empty_level_timeout`, else
`Net.empty_level_timeout`, else the project setting `gdnet/levels/empty_timeout_s` (default 30). Its entities
are destroyed first. The next `travel` (or `load_level`) loads it again, with fresh entities.

**Moving an entity between managers by hand** (`remove_child` + `add_child`) is still a destroy and a
create; `Net.travel` is the move.

## Chunk overworlds

A large outdoor area is a grid of chunk scenes. Each chunk's `NetManager` names the chunks next to it in
`neighbours`:

```
[node name="NetManager" type="NetManager" parent="."]
neighbours = PackedStringArray("res://world/chunk_1_0.tscn", "res://world/chunk_0_1.tscn", "res://world/chunk_1_1.tscn")
auto_travel = true
bounds = Rect2(0, 0, 2048, 2048)
```

A player in `chunk_0_0` then has the rooms of that chunk and its three neighbours: its client loads all
four levels, sees their entities and collides with their geometry, so there is no hard edge. When the
player crosses into `chunk_1_0`, that is a move between two rooms it already has (only the `Reparent`); the
chunks that stop being neighbours are left (`RoomLeft`, freed on the client) and the new neighbours
arrive (`Travel`). The server loads a neighbour that is not loaded yet by itself.

Keep chunks small: every neighbour costs the client its memory and load time.

The crossing itself is a script at the edge calling `Net.travel(body, next_chunk)` without a spawn point,
or `auto_travel`, below.

## Placement

Levels loaded at the same time overlap at the origin unless they are placed apart. Each loaded level has
an **origin**, the same on the server and on every client (it travels in `Travel`):

```gdscript
Net.load_level("res://levels/house.tscn", Vector2(-3000, 800))   # a Vector3 for a 3D level
Net.level_origin("res://levels/house.tscn")                     # Vector2(-3000, 800); null when authored
```

The level's root is put at the origin, and the `Node2D` / `Node3D` children of its managers (which do not
inherit the root's transform, a `NetManager` being a plain `Node`) move by the same offset, so the whole
level moves as the editor shows it. Spawn markers follow, since they are read at their global position.

**Automatic placement.** Set the project setting `gdnet/levels/spacing` (default 0 = off) to a distance
larger than your largest level. A level then loaded without an origin (by `load_level`, `travel`,
`add_room` or as a neighbour) takes the next free cell of a grid: cell n sits at x = n × spacing (n = 1,
2, ...; the origin is left to the authored world), and the cell of an unloaded level is used again. A
level whose manager names `neighbours` is a chunk and keeps the position it was authored at.

With placement on, a server holding several interiors needs no collision layer per level, and a door no
longer has to check that a body belongs to its own level: bodies of different levels are far apart. (The
headless tests run several peers in one process, which shares one physics space between the peers'
copies of a level; that is a test setup, not a game's.)

## Bounds and auto-travel

`NetManager.bounds` (a `Rect2` for 2D) or `bounds_3d` (an `AABB` for 3D) is the area of the manager's room,
in the coordinates of its level's root, so it moves with the level's origin. With `auto_travel = true`,
on the server each tick, a player entity whose root stands inside that area (and outside its own
manager's bounds) travels to that manager, keeping its global position. A chunk overworld needs no edge
scripts: give every chunk its bounds and `auto_travel`.

## Level state

*Placeholder for WP16 slice C (`keep_state`, `level_state`, `reset_level`, the `state` argument of
`load_level`).*

## A headless server

```
godot --headless --path . demos/doors/main.tscn -- --server
```

Everything after `--` reaches the game through `OS.get_cmdline_user_args()`; the demo's `main.gd` hosts
without a local player when it sees `--server`. To try a server and two clients from the editor: *Debug >
Customize Run Instances...*, *Enable Multiple Instances*, 3 instances, launch arguments `-- --server` on
the first; run the scene and press Join in the other two windows.

## Limits

* Rooms are all or nothing per manager; distance-based interest inside a room is page 11's `radius`.
* A `Travel` carries the scene path; the client must have the same scene at the same path (as for every
  networked scene).
* Only players auto-travel; other entities move between managers with `Net.travel`.
* A level holding a player owned by the server (the host's own player) is never unloaded.
* The physics tick of a moved entity runs after the other nodes' in the tick of the move (Godot skips a
  node that leaves the tree during the physics pass, and gdnet runs that tick for it afterwards).
